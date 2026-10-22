//! `aa-viewer` command: everything lives in the library (`lib.rs`) so the
//! iPhone app can start the same viewer from its own `main`.

fn main() -> anyhow::Result<()> {
    aa_viewer::run_cli()
}
