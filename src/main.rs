use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

fn main() {
    pretty_env_logger::formatted_timed_builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .init();
    xwayland_satellite::main(parse_args());
}

#[derive(Default)]
struct RealData {
    display: Option<String>,
    listenfds: Vec<OwnedFd>,
    flags: Vec<String>,
}
impl xwayland_satellite::RunData for RealData {
    fn display(&self) -> Option<&str> {
        self.display.as_deref()
    }

    fn listenfds(&mut self) -> Vec<OwnedFd> {
        std::mem::take(&mut self.listenfds)
    }

    fn flags(&self) -> &[String] {
        &self.flags
    }
}

struct ParsedFlags {
    disable_ac: bool,
    audit_level: u32,
    auth_file: Option<String>,
    coredump: bool,
    extension_plus: Vec<String>,
    extension_minus: Vec<String>,
    glamor: Option<&'static str>,
    listen_plus: Vec<String>,
    listen_minus: Vec<String>,
    verbosity: u32,
}
impl Default for ParsedFlags {
    fn default() -> Self {
        Self {
            disable_ac: false,
            audit_level: 1,
            auth_file: None,
            coredump: false,
            extension_plus: vec![],
            extension_minus: vec![],
            glamor: None,
            listen_plus: vec![],
            listen_minus: vec![],
            verbosity: 0,
        }
    }
}
impl ParsedFlags {
    fn to_vec(&self) -> Vec<String> {
        let mut ret: Vec<&str> = vec![];
        if self.disable_ac {
            ret.push("-ac");
        }
        let audit = self.audit_level.to_string();
        ret.extend(["-audit", &audit]);
        if let Some(ref auth) = self.auth_file {
            ret.extend(["-auth", auth]);
        }
        if self.coredump {
            ret.push("-core");
        }
        for ext in self.extension_plus.iter() {
            ret.extend(["+extension", ext]);
        }
        for ext in self.extension_minus.iter() {
            ret.extend(["-extension", ext]);
        }
        if let Some(glamor) = self.glamor {
            ret.extend(glamor.split_ascii_whitespace());
        }
        for protocol in self.listen_plus.iter() {
            ret.extend(["-listen", protocol]);
        }
        for protocol in self.listen_minus.iter() {
            ret.extend(["-nolisten", protocol]);
        }
        let verbosity = self.verbosity.to_string();
        ret.extend(["-verbose", &verbosity]);
        ret.into_iter().map(str::to_string).collect()
    }
}

fn parse_args() -> RealData {
    let mut data = RealData::default();
    let mut flags = ParsedFlags::default();
    let mut args = std::env::args().skip(1).peekable();

    // The first argument (other than the skipped-over binary name) is optionally the display name.
    let Some(arg) = args.peek() else {
        return data;
    };
    if arg.starts_with(':') {
        data.display = Some(arg.to_owned());
        args.next();
    }

    // All other options (including the first if it was not a display name) are supported flags
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-ac" => {
                flags.disable_ac = true;
            }
            "-audit" => {
                flags.audit_level = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .expect("argument to -audit not provided or integer");
            }
            "-auth" => {
                // X.org lets you pass multiple `-auth` parameters but only uses the last one.
                // This is unintuitive enough that passing multiple `-auth` should be an error.
                if flags.auth_file.is_some() {
                    panic!("Multiple `-auth` flags passed");
                }
                let Some(ref file) = args.next() else {
                    panic!("No authorization file passed");
                };
                std::fs::OpenOptions::new()
                    .read(true)
                    .open(file)
                    .expect("Could not open authorization file");
                flags.auth_file = Some(file.to_owned());
            }
            "-core" => {
                flags.coredump = true;
            }
            "+extension" => {
                let ext = args.next().expect("argument to +extension not provided");
                if let Some(idx) = flags.extension_minus.iter().position(|e| *e == ext) {
                    flags.extension_minus.swap_remove(idx);
                }
                flags.extension_plus.push(ext.to_owned());
            }
            "-extension" => {
                let ext = args.next().expect("argument to -extension not provided");
                // Do not disable essential extensions (see XState::new for this list)
                if !["COMPOSITE", "RANDR", "XFIXES", "X-Resource"].contains(&&ext[..]) {
                    if let Some(idx) = flags.extension_plus.iter().position(|e| *e == ext) {
                        flags.extension_plus.swap_remove(idx);
                    }
                    flags.extension_minus.push(ext.to_owned());
                }
            }
            "-glamor" => {
                let api = args.next().expect("argument to -glamor not provided");
                match &api[..] {
                    "gl" => flags.glamor = Some("-glamor gl"),
                    "es" => flags.glamor = Some("-glamor es"),
                    // For maximum compatability with Xwayland compiled without Glamor support, this
                    // is equivalent to -glamor none and is always available
                    "none" => flags.glamor = Some("-shm"),
                    e => panic!("unknown rendering API passed: {e}"),
                };
            }
            "-help" => {
                // Wording for most help messages taken directly from Xwayland
                println!("use: xwayland-satellite [:<display>] [option]");
                println!("{:<25} disable access control restrictions", "-ac");
                println!("{:<25} set audit trail level", "-audit <int>");
                println!(
                    "{:<25} generate core dump of Xwayland on fatal error",
                    "-core"
                );
                println!("{:<25} Enable extension", "+extension name");
                println!("{:<25} Disable extension", "-extension name");
                println!(
                    "{:<25} use given API for Glamor acceleration",
                    "-glamor [gl|es|none]"
                );
                println!("{:<25} prints message with these options", "-help");
                println!("{:<25} listen on protocol", "-listen");
                println!("{:<25} don't listen on protocol", "-nolisten");
                println!("{:<25} add given fd as a listen socket", "-listenfd");
                println!(
                    "{:<25} return 0 if supports -listenfd",
                    "--test-listenfd-support"
                );
                println!("{:<25} verbose startup messages", "-verbose [n]");
                println!(
                    "{:<25} show the xwayland-satellite version and exit",
                    "-version"
                );
                std::process::exit(0);
            }
            "-listen" => {
                let protocol = args.next().expect("argument to -listen not provided");
                if let Some(idx) = flags.listen_minus.iter().position(|p| *p == protocol) {
                    flags.listen_minus.swap_remove(idx);
                }
                flags.listen_plus.push(protocol.to_owned());
            }
            "-nolisten" => {
                let protocol = args.next().expect("argument to -nolisten not provided");
                if let Some(idx) = flags.listen_plus.iter().position(|p| *p == protocol) {
                    flags.listen_plus.swap_remove(idx);
                }
                flags.listen_minus.push(protocol.to_owned());
            }
            "-listenfd" => {
                let fd: RawFd = args
                    .next()
                    .expect("Required argument to -listenfd not specified")
                    .parse()
                    .expect("Error parsing -listenfd number");
                // SAFETY:
                // - whoever runs the binary must ensure this fd is open and valid.
                // - parse_args() must only be called once to avoid double closing.
                // - no fd can be provided multiple times to avoid double closing.
                assert!(
                    !data.listenfds.iter().any(|l| l.as_raw_fd() == fd),
                    "Multiple -listenfd with the same fd is not allowed"
                );
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                data.listenfds.push(fd);
            }
            "--test-listenfd-support" => std::process::exit(0),
            "-verbose" => {
                if let Some(v) = args.peek().and_then(|n| n.parse::<u32>().ok()) {
                    flags.verbosity = v;
                    args.next();
                } else {
                    flags.verbosity += 1;
                }
            }
            "-version" => {
                println!("{}", xwayland_satellite::version());
                std::process::exit(0);
            }
            _ => {
                panic!("Unrecognized argument: {arg}");
            }
        }
    }
    data.flags = flags.to_vec();

    data
}
