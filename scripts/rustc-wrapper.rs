use std::{
    env,
    ffi::OsString,
    process::{exit, Command},
};

fn normalize(value: OsString) -> OsString {
    let value = value.to_string_lossy().into_owned();
    value
        .replace(r"\\?\UNC\", r"\\")
        .replace(r"\\?\", "")
        .into()
}

fn main() {
    let mut args = env::args_os();
    let _wrapper = args.next();

    let Some(rustc) = args.next() else {
        eprintln!("rustc-wrapper expected rustc as its first argument");
        exit(1);
    };

    let mut command = Command::new(normalize(rustc));

    for name in [
        "OUT_DIR",
        "CARGO_MANIFEST_DIR",
        "CARGO_MANIFEST_PATH",
        "CARGO_TARGET_DIR",
    ] {
        if let Some(value) = env::var_os(name) {
            command.env(name, normalize(value));
        }
    }

    command.args(args.map(normalize));

    match command.status() {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("failed to run rustc: {error}");
            exit(1);
        }
    }
}
