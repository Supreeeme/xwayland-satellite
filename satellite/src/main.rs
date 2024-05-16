fn main() {
    pretty_env_logger::formatted_timed_builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .init();
    xwayland_satellite::main(RealData(get_display()));
}

#[repr(transparent)]
struct RealData(Option<String>);
impl xwayland_satellite::RunData for RealData {
    fn display(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

fn get_display() -> Option<String> {
    let mut args: Vec<_> = std::env::args().collect();
    if args.len() > 2 {
        panic!("Unexpected arguments: {:?}", &args[2..]);
    }

    (args.len() == 2).then(|| args.swap_remove(1))
}
