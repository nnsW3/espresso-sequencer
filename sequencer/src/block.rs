mod full_payload;
mod namespace_payload;
mod uint_bytes;

pub use full_payload::{NsProof, NsTable, Payload};

#[cfg(test)]
mod test;
