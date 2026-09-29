//! Embeds the build identifier as `DAGQ_BUILD_ID`, by dagq's own rule and
//! from the same checkout (the repository root, two levels up), so this
//! binary names the same build as the `dagq` built with it (ADR-t827-1
//! decisions 5 and 7).

fn main() {
    dagq_broker_protocol::build_id::emit("../..");
}
