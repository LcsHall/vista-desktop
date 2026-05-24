// Prevents an extra console window from spawning when launching a
// release build on Windows. (Dev builds still show the console so
// we can see println! / log output.)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    vista_platform_lib::run()
}
