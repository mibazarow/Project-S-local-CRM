// Public schema entry points. The migration bodies and the ordered registry
// live in db::migrations; this module keeps the stable public paths used by
// the rest of the app (db::schema::SCHEMA_VERSION, db::schema::run_migrations).
pub const SCHEMA_VERSION: i64 = 41;

pub use crate::db::migrations::run_migrations;
