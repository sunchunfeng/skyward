//! AVB and dm-verity: what the bootloader holds and changing it.

use super::*;
use skyward_install::avb::State as AvbPartition;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvbStatus {
    pub unlocked: bool,
    pub partitions: Vec<AvbPartition>,
}

impl App {
    pub fn set_avb(&mut self, status: Result<AvbStatus, String>) {
        if let Ok(status) = &status {
            self.state.avb_disable_verity = !status.partitions.is_empty()
                && status.partitions.iter().all(|p| !p.verity_enabled());
        }
        self.state.avb = Some(status);
        self.refresh();
    }

    pub fn avb_changed(&mut self, result: Result<AvbStatus, String>) {
        match result {
            Ok(status) => {
                self.set_avb(Ok(status));
                self.state.avb_result = Some(Ok(()));
            }
            Err(why) => self.state.avb_result = Some(Err(why)),
        }
        self.refresh();
    }
}