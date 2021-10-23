pub mod dovi_rpu;
pub mod rpu_data_header;
pub mod rpu_data_mapping;
pub mod rpu_data_nlq;
pub mod vdr_dm_data;

use crc::{Crc, CRC_32_MPEG_2};

pub const NUM_COMPONENTS: usize = 3;

#[inline(always)]
fn compute_crc32(data: &[u8]) -> u32 {
    let crc = Crc::<u32>::new(&CRC_32_MPEG_2);
    let mut digest = crc.digest();
    digest.update(data);

    digest.finalize()
}
