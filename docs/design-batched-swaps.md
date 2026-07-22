# Design: batched component swaps + explicit-v0 client tx build

## Goal
Cut the large-basket mint/redeem from ~23 (or ~12) transactions down to a handful,
by (a) fixing client-side ALT compression and (b) batching multiple component
swaps per transaction.

## Findings that shape the design (measured, mainnet/live route, XTEST5)
1. **App send path doesn't compress.** `@solana/client`'s `prepare()` transaction
   planner sizes instructions WITHOUT crediting ALT compression, so it rejects the
   execute at 1395 B even though the compiled message is 1062 B and fits. `@solana/kit`'s
   compiler DOES compress `IAccountLookupMeta` (verified: 20 accounts -> 210 B).
   => Build large-basket txs as EXPLICIT compiled v0 messages with the ALT, sign/send
   via the wallet, bypassing the planner's faulty sizing.
2. **Direct swaps are far smaller than wrapped ones.** N standalone Jupiter swaps,
   compressed with all ALTs: 1->461, 2->617, 3->837, 4->1025, 5->1212 B. So ~5 fit
   by bytes. Our current execute is 1062 B for ONE swap because the program encodes
   the swap as `JupiterSwapPlan` DATA (duplicated mints/token-accounts = ~128 B of
   incompressible data per swap on top of the appended route accounts).
3. **CU is the real ceiling (UNMEASURED — must measure on the deployed program).**
   Jupiter swaps ~250-400K CU each; tx limit 1.4M => ~3-5 swaps/tx. Batch size must
   be CONFIGURABLE and conservative until measured against the real CPI path.

## Realistic outcome
With batch size B (gated by CU, likely 3-5): mint = open + ceil(10/B) execute + collect
+ finalize. B=5 -> ~4 txs; B=3 -> ~6 txs. NOT 2, but a large win over 23/12. Set
expectation with the user.

## Program: batched execute
New `execute_large_basket_mint_components` (plural) takes a Vec of per-component
entries: { component_index, max_quote_in, swap: JupiterSwapPlan, route_account_range }.
remaining_accounts hold the concatenated route accounts for all entries; each entry
carries an (offset, len) into that slice. For each entry: validate route-account
scope (the existing per-swap guard, applied to its slice), CPI swap, measure
quote_spent/received, store per-component, mark filled, run the budget guard.
- Keep the deferred price-check (verify_* instruction) OR fold a batched verify —
  decide after CU: if a batch of B swaps + B single-feed quotes fits CU+bytes, a
  batched verify (B<=8 feeds in one quote) is cleanest. Otherwise keep separate.
- Account-scope validation MUST be per-entry (a malicious route in entry i must not
  reach entry j's vault or any protocol account). Reuse validate_jupiter_route_account_scope
  per slice.
- Compute-unit guard: the instruction can't self-limit CU, so the CLIENT sets the
  batch size; the program just processes whatever it's given. Document the safe max.

## Client: explicit v0 build
- New builder path for large-basket mint/redeem: assemble instruction groups, then
  compile each group to a v0 message with the resolved ALTs (fetch ALT account data,
  compileToV0Message), and hand the compiled message to the wallet for signing —
  NOT through `solanaClient.transaction.prepare()`.
- Group plan: [open] [batched-execute x ceil(N/B)] [verify x ...] [collect] [finalize].
- Move the 10 ATA-create preInstructions OUT of the first execute (they belong with
  open or their own tx).

## Per-entry candidate-ordering contract (SECURITY-CRITICAL)
Stage 1 (DONE): `execute_mint_swap_inner(jupiter_program, owner, quote_mint,
owner_quote_token_account, component_vault, candidates, swap, component, required)`
— account-parameterized, 82 tests green. The single execute delegates to it.

For the batched instruction, each entry's `candidates` list (the accounts its
Jupiter route may reference, by index) MUST be built from ONLY:
  [fixed shared accounts: owner, index, index_mint, intent, vault_authority,
   quote_mint, owner_quote_token_account, jupiter_program, associated_token_program,
   quote_token_program, system_program]
  ++ [this entry's component accounts: component_page, component_mint, component_vault,
      component_token_program]
  ++ [this entry's route accounts only]
NEVER another entry's vault/accounts. The swap.accounts indices in each entry are
relative to THAT entry's candidates ordering. validate_jupiter_route_account_scope
is called per entry with writable whitelist = [this entry's component_vault]. This is
what prevents a malicious route in entry i from touching entry j's funds.

remaining_accounts layout: concatenated per-entry groups, each =
[component_page, component_mint, component_vault, component_token_program, ...route].
Each arg entry carries route_account_count so the handler can slice with a cursor.

## Client implementation contract (Stage 6 — remaining)
Decided: DROP per-component verify (rely on Jupiter slippage + aggregate
max_quote_in/min_quote_out). Program finalize gates removed. Flow:
open → execute_batch ×ceil(N/B) → collect → finalize (~7 txs at B=3).

### Batched execute assembly (protocol-transactions.ts)
- Chunk componentPlans into batches of B (start B=3, CU-gated; tune in Stage 7).
- Per batch, ONE getExecuteLargeBasketMintBatchInstructionAsync call:
  fixed accounts owner/index/indexMint/intent/ownerQuoteTokenAccount/jupiterProgram
  (vaultAuthority/quoteMint/associatedTokenProgram/systemProgram default-resolve;
  PASS vaultAuthority explicitly if the resolver isn't generated).
  args.entries[i] = { componentIndex, maxQuoteIn: BigInt(plan.maxQuoteIn),
  routeAccountCount: plan.routeAccounts.length, swap: jupiterSwapPlanFromComponentPlan(plan) }.
- Then appendAccounts(batchIx, remaining) where `remaining` is the CONCATENATION,
  per entry in batch order, of EXACTLY: [componentPage(RO), componentMint(RO),
  componentVault(WRITABLE), componentTokenProgram(RO), ...plan.routeAccounts (mapped
  via accountMetaFromSerialized, PLAIN metas)]. Order is load-bearing: the program
  slices [page,mint,vault,tokprog] then routeAccountCount route accounts per entry.
- Redeem: same, but use the redeem batch builder + min_quote_out; component metas
  same; vault WRITABLE (swap sells from it).
- swap.accounts indices already match the program candidate ordering (server
  compaction aligned to it this session). Do NOT remap.
- Move the 10 ATA-create preInstructions OUT of execute (put them in their own
  tx before the first batch, or rely on the program's idempotent vault create —
  the program already creates the vault ATA per entry, so the preInstructions may
  be droppable for large baskets; verify).

### Explicit-v0 compressed send (live-protocol.tsx) — REQUIRED, replaces prepare()
@solana/client prepare() does NOT compress (planner sizes uncompressed → 1395
reject). For mint/redeem, build + send each tx group manually with kit:
  msg = pipe(createTransactionMessage({version:0}),
             m=>setTransactionMessageFeePayerSigner(walletSigner, m),
             m=>setTransactionMessageLifetimeUsingBlockhash(blockhash, m),
             m=>appendTransactionMessageInstructions(ixs, m),
             m=>compressTransactionMessageUsingAddressLookupTables(m, altMap))
  altMap: AddressesByLookupTableAddress = payload.lookupTables (Record<addr,addr[]>).
  Then EITHER signTransactionMessageWithSigners(msg)+rpc.sendTransaction, OR wrap as
  a TransactionPrepared { commitment:'confirmed', feePayer, instructions:ixs,
  lifetime:{blockhash,lastValidBlockHeight}, message:msg, mode:'send', version:0 }
  and call solanaClient.transaction.send(prepared).
  blockhash: from useLatestBlockhash() at hook top, threaded into the send callback.
- UNTESTABLE headless: needs browser+wallet. Validate each piece live.

## Open risks
- Batched Jupiter CPIs: partial failure semantics (entry 3 of 5 fails -> whole tx
  reverts, fine, but intent state must be all-or-nothing per tx — it is, tx atomic).
- Wallet signing of raw compiled v0 messages (vs the client's prepare helper).
- This is the largest change in the project and touches the money path. Stage:
  program -> cargo test -> client -> build -> measure CU on a real execute -> tune B.
