use metrics::{
    log_counter_with_labels,
    register_convex_counter,
    StaticMetricLabel,
    STATUS_LABEL,
};

register_convex_counter!(
    DATABASE_REPLICA_BOOTSTRAP_TOTAL,
    "Number of fresh replica checkpoint bootstrap attempts by status",
    &STATUS_LABEL
);

pub fn log_replica_bootstrap_result(success: bool) {
    log_counter_with_labels(
        &DATABASE_REPLICA_BOOTSTRAP_TOTAL,
        1,
        vec![StaticMetricLabel::status(success)],
    );
}
