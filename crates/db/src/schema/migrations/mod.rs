mod v02_v20;
mod v21_v31;
mod v32_v36;
mod v37_v42;
mod v43_v47;
mod v48;
mod v49;
mod v50;

use super::Migration;

pub(super) static MIGRATION_GROUPS: &[&[Migration]] = &[
    v02_v20::MIGRATIONS,
    v21_v31::MIGRATIONS,
    v32_v36::MIGRATIONS,
    v37_v42::MIGRATIONS,
    v43_v47::MIGRATIONS,
    v48::MIGRATIONS,
    v49::MIGRATIONS,
    v50::MIGRATIONS,
];
