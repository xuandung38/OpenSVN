//! wc.db access layer (track A). [`WcDb`] is the only way other modules touch the database.

/// Handle to one working copy's `wc.db`: a single write connection plus busy-retrying reads.
///
/// Fields and constructors arrive with the schema milestone.
#[derive(Debug)]
pub struct WcDb {
    _private: (),
}
