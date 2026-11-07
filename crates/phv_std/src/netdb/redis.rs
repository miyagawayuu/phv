// ----- Redis -----------------------------------------------------------------

use super::pool::Driver;
use super::RESET_TIMEOUT;
use crate::redisclient::RedisConn;
use phv_vm::DatabaseSpec;
use std::time::{Duration, Instant};

impl Driver for RedisConn {
    fn reset(&mut self) -> bool {
        // Scripts can only run the data commands below, which leave no session state.
        true
    }

    fn check(&mut self) -> bool {
        self.command(&[b"PING"], Instant::now() + RESET_TIMEOUT, 1024).is_ok()
    }
}

pub(super) fn redis_connect(url: &str, spec: &DatabaseSpec, timeout: Duration) -> Result<RedisConn, String> {
    RedisConn::connect(url, spec.tls, Instant::now() + timeout)
}
