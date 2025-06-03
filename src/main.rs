use std::os::fd::{FromRawFd, OwnedFd, RawFd};

fn main() {
    pretty_env_logger::formatted_timed_builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .init();
    xwayland_satellite::main(parse_args());
}

struct RealData {
    display: Option<String>,
    listenfds: Vec<OwnedFd>,
}
impl xwayland_satellite::RunData for RealData {
    fn display(&self) -> Option<&str> {
        self.display.as_deref()
    }

    fn listenfds(&mut self) -> Vec<OwnedFd> {
        std::mem::take(&mut self.listenfds)
    }
}

fn parse_args() -> RealData {
    let mut data = RealData {
        display: None,
        listenfds: Vec::new(),
    };

    let mut args: Vec<_> = std::env::args().collect();
    if args.len() < 2 {
        return data;
    }

    // Argument at index 1 is our display name. The rest can be -listenfd.
    let mut i = 2;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-listenfd" {
            let next = i + 1;
            if next == args.len() {
                // Matches the Xwayland error message.
                panic!("Required argument to -listenfd not specified");
            }

            let fd: RawFd = args[next].parse().expect("Error parsing -listenfd number");
            // SAFETY:
            // - whoever runs the binary must ensure this fd is open and valid.
            // - parse_args() must only be called once to avoid double closing.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };

            data.listenfds.push(fd);
            i += 2;
        } else if arg == "--test-listenfd-support" {
            std::process::exit(0);
        } else {
            panic!("Unrecognized argument: {arg}");
        }
    }

    data.display = Some(args.swap_remove(1));

    data
}
