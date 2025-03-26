use smithay_client_toolkit::{
    delegate_shm,
    shm::{Shm, ShmHandler},
};

use super::Globals;

delegate_shm!(Globals);
impl ShmHandler for Globals {
    fn shm_state(&mut self) -> &mut Shm {
        // &self.shm
        todo!()
    }
}
