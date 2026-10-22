// Anywhere for iPhone: everything is in Rust (crates/aa-viewer, ios.rs).
// This file only hands the main thread to it; it never returns.
extern void aa_viewer_ios_main(void);

int main(int argc, char *argv[]) {
    (void)argc;
    (void)argv;
    aa_viewer_ios_main();
    return 0;
}
