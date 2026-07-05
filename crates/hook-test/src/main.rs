use std::thread;
use std::time::Duration;

// DLL関数の宣言
#[link(name = "hook_dll", kind = "dylib")]
extern "C" {
    fn install_hook() -> bool;
    fn uninstall_hook() -> bool;
}

fn main() {
    println!("Installing keyboard hook...");
    
    unsafe {
        if install_hook() {
            println!("Hook installed! Press keys to see output.");
            println!("Press Ctrl+C to exit.");
            
            // メッセージループ（10秒間）
            for i in 0..10 {
                println!("Running... {}s", i + 1);
                thread::sleep(Duration::from_secs(1));
            }
            
            uninstall_hook();
            println!("Hook uninstalled.");
        } else {
            eprintln!("Failed to install hook!");
        }
    }
}