use super::XState;
use log::warn;
use xcb::x;

impl XState {
    pub(super) fn update_xft_dpi_resource(&self, dpi: i32) {
        // Other clients may replace the resource database so don't cache its contents
        let reply = self
            .connection
            .wait_for_reply(self.connection.send_request(&x::GetProperty {
                delete: false,
                window: self.root,
                property: self.atoms.resource_manager,
                r#type: x::ATOM_STRING,
                long_offset: 0,
                long_length: u32::MAX,
            }))
            .unwrap();

        let resources = match reply.r#type() {
            x::ATOM_NONE => &[],
            x::ATOM_STRING => reply.value::<u8>(),
            other => {
                warn!("RESOURCE_MANAGER has unexpected type {other:?}");
                return;
            }
        };

        let xft_dpi = format!("Xft.dpi:\t{dpi}");
        let mut updated = Vec::with_capacity(resources.len() + xft_dpi.len() + 1);
        let mut replaced = false;

        for line in resources.split_inclusive(|byte| *byte == b'\n') {
            let resource_name = line
                .iter()
                .position(|byte| *byte == b':')
                .map(|separator| line[..separator].trim_ascii());
            if resource_name == Some(b"Xft.dpi") {
                if !replaced {
                    updated.extend_from_slice(xft_dpi.as_bytes());
                    if line.ends_with(b"\n") {
                        updated.push(b'\n');
                    }
                    replaced = true;
                }
            } else {
                updated.extend_from_slice(line);
            }
        }

        if !replaced {
            if !updated.is_empty() && !updated.ends_with(b"\n") {
                updated.push(b'\n');
            }
            updated.extend_from_slice(xft_dpi.as_bytes());
            updated.push(b'\n');
        }

        self.connection
            .send_and_check_request(&x::ChangeProperty {
                window: self.root,
                mode: x::PropMode::Replace,
                property: self.atoms.resource_manager,
                r#type: x::ATOM_STRING,
                data: &updated,
            })
            .unwrap();
    }
}
