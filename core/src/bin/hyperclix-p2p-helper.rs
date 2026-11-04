fn main() {
    if let Err(error) = hyperclix_core::proximity::p2p_helper::run() {
        eprintln!("[p2p-helper] {error:#}");
        std::process::exit(1);
    }
}
