fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(cmh::kernel::cli::main(args));
}
