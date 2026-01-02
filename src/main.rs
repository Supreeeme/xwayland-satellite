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
    disable_ac: bool,
    audit_level: u32,
    auth_file: Option<String>,
    coredump: bool,
    extension_plus: Vec<String>,
    extension_minus: Vec<String>,
    listen_plus: Vec<String>,
    listen_minus: Vec<String>,
    verbosity: u32,
}
impl Default for RealData {
    fn default() -> Self {
        Self {
            display: None,
            listenfds: vec![],
            disable_ac: false,
            audit_level: 1,
            auth_file: None,
            coredump: false,
            extension_plus: vec![],
            extension_minus: vec![],
            listen_plus: vec![],
            listen_minus: vec![],
            verbosity: 0,
        }
    }
}
impl xwayland_satellite::RunData for RealData {
    fn display(&self) -> Option<&str> {
        self.display.as_deref()
    }

    fn listenfds(&mut self) -> Vec<OwnedFd> {
        std::mem::take(&mut self.listenfds)
    }

    fn flags(&self) -> Vec<String> {
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
    let args: Vec<_> = std::env::args().collect();

    // The first argument is optionally the display name.
    let Some(arg) = args.get(1) else {
        return data;
    };
    let mut i = if arg.starts_with(':') {
        data.display = Some(arg.to_owned());
        2
    } else {
        1
    };

    // All other options (including the first if it was not a display name) are supported flags
    while let Some(arg) = args.get(i) {
        match arg.as_str() {
            "-ac" => {
                data.disable_ac = true;
                i += 1;
            }
            "-audit" => {
                data.audit_level = args
                    .get(i + 1)
                    .and_then(|n| n.parse().ok())
                    .expect("argument to -audit not provided or integer");
                i += 2;
            }
            "-auth" => {
                // X.org lets you pass multiple `-auth` parameters but only uses the last one.
                // This is unintuitive enough that passing multiple `-auth` should be an error.
                if data.auth_file.is_some() {
                    panic!("Multiple `-auth` flags passed");
                }
                let Some(file) = args.get(i + 1) else {
                    panic!("No authorization file passed");
                };
                std::fs::OpenOptions::new()
                    .read(true)
                    .open(file)
                    .expect("Could not open authorization file");
                data.auth_file = Some(file.to_owned());
                i += 2;
            }
            "-core" => {
                data.coredump = true;
                i += 1;
            }
            "+extension" => {
                let ext = args
                    .get(i + 1)
                    .expect("argument to +extension not provided");
                if let Some(idx) = data.extension_minus.iter().position(|e| e == ext) {
                    data.extension_minus.swap_remove(idx);
                }
                data.extension_plus.push(ext.to_owned());
                i += 2;
            }
            "-extension" => {
                let ext = args
                    .get(i + 1)
                    .expect("argument to -extension not provided");
                // Do not disable essential extensions (see XState::new for this list)
                if !["COMPOSITE", "RANDR", "XFIXES", "X-Resource"].contains(&&ext[..]) {
                    if let Some(idx) = data.extension_plus.iter().position(|e| e == ext) {
                        data.extension_plus.swap_remove(idx);
                    }
                    data.extension_minus.push(ext.to_owned());
                }
                i += 2;
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
                let protocol = args
                    .get(i + 1)
                    .expect("argument to -extension not provided");
                if let Some(idx) = data.listen_minus.iter().position(|p| p == protocol) {
                    data.listen_minus.swap_remove(idx);
                }
                data.listen_plus.push(protocol.to_owned());
                i += 2;
            }
            "-nolisten" => {
                let protocol = args
                    .get(i + 1)
                    .expect("argument to -extension not provided");
                if let Some(idx) = data.listen_plus.iter().position(|p| p == protocol) {
                    data.listen_plus.swap_remove(idx);
                }
                data.listen_minus.push(protocol.to_owned());
                i += 2;
            }
            "-listenfd" => {
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
            }
            "--test-listenfd-support" => std::process::exit(0),
            "-verbose" => {
                if let Some(v) = args.get(i + 1).and_then(|n| n.parse::<u32>().ok()) {
                    data.verbosity = v;
                    i += 2;
                } else {
                    data.verbosity += 1;
                    i += 1;
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

    data
}
