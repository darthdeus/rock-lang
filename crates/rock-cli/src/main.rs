#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    rock_cli::main_with(&rock_cli::plain::REGISTRATIONS);
}
