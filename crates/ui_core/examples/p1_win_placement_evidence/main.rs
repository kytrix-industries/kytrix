#[cfg(windows)]
mod evidence;

#[cfg(not(windows))]
fn main() {
    eprintln!("p1_win_placement_evidence 只在 Windows 上有意义：本平台跳过");
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    evidence::run()
}
