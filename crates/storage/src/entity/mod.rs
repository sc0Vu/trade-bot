//! SeaORM entities. Crate-private: callers use the repository traits.
//! Column layout must match `migration/`.

pub(crate) mod holding;
pub(crate) mod meta;
pub(crate) mod order;
pub(crate) mod trade;
