//! Request/response correlation and connection lifecycle state machine for the Navi browser link.
//!
//! Ported from `packages/navi/src/core/connection.ts`. This module owns no I/O: callers register
//! outbound requests, feed inbound responses back, and drive expiry from a monotonic millisecond
//! clock. Request ids start at 1, increase by one, and stop at a per-connection budget so an
//! exhausted link fails closed instead of growing an unbounded pending map. Settled ids are
//! remembered in a bounded ring, which distinguishes a late duplicate from a never-seen id.

use navi_types::{NaviError, NaviResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};

/// Lifecycle of one logical browser link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    /// No link, or the link has been torn down.
    Disconnected,
    /// Transport is opening and the handshake has not finished.
    Connecting,
    /// Handshake complete; requests may be issued.
    Connected,
    /// Close was requested and in-flight work is draining.
    Closing,
}

impl ConnectionState {
    /// Returns true when `next` is a legal successor of `self`.
    ///
    /// Re-asserting the same state is allowed so callers can make open/close idempotent.
    pub fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        matches!(
            (self, next),
            (Self::Disconnected, Self::Connecting)
                | (Self::Connecting, Self::Connected | Self::Disconnected)
                | (Self::Connected, Self::Closing | Self::Disconnected)
                | (Self::Closing, Self::Disconnected)
        )
    }

    /// Returns true when new requests may be issued in this state.
    pub fn accepts_requests(self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// A single outbound request awaiting its response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRequest {
    /// Monotonically increasing correlation id.
    pub id: u64,
    /// Protocol operation name, for example `observe`, `focus`, `scroll`, or `cancel`.
    pub method: String,
    /// Monotonic clock reading, in milliseconds, when the request was registered.
    pub created_at_ms: u64,
    /// Relative deadline in milliseconds.
    pub timeout_ms: u64,
}

impl PendingRequest {
    /// Returns the absolute millisecond timestamp at which this request times out.
    pub fn deadline_ms(&self) -> u64 {
        self.created_at_ms.saturating_add(self.timeout_ms)
    }

    /// Returns true once the deadline has been reached.
    pub fn is_expired_at(&self, now_ms: u64) -> bool {
        now_ms >= self.deadline_ms()
    }
}

/// The peer's answer to a registered request, kept schema-agnostic for the transport layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// A successful result payload.
    Success(Value),
    /// A peer-reported error.
    Error(NaviError),
}

/// A response paired with the request it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettledResponse {
    /// The request removed from the pending set.
    pub request: PendingRequest,
    /// The answer delivered by the peer.
    pub response: Response,
}

/// Bounds for one connection. Defaults mirror `DEFAULT_CONNECTION_LIMITS` in `connection.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLimits {
    pub handshake_timeout_ms: u64,
    pub observe_timeout_ms: u64,
    pub interaction_timeout_ms: u64,
    /// Recently settled ids kept per direction so duplicate and late results are detectable.
    pub tracked_requests: usize,
    /// Distinct request ids accepted on one connection before it must be reopened.
    pub max_request_ids_per_connection: u64,
    pub reconnect_base_delay_ms: u64,
    pub reconnect_max_delay_ms: u64,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            handshake_timeout_ms: 5_000,
            observe_timeout_ms: 4_000,
            interaction_timeout_ms: 2_000,
            tracked_requests: 32,
            max_request_ids_per_connection: 512,
            reconnect_base_delay_ms: 100,
            reconnect_max_delay_ms: 30_000,
        }
    }
}

impl ConnectionLimits {
    /// Returns the absolute handshake deadline for a handshake started at `started_at_ms`.
    pub fn handshake_deadline_ms(&self, started_at_ms: u64) -> u64 {
        started_at_ms.saturating_add(self.handshake_timeout_ms)
    }

    /// Returns the default deadline for a method; reads use the longer observation timeout.
    pub fn timeout_for(&self, method: &str) -> u64 {
        if method == "observe" {
            self.observe_timeout_ms
        } else {
            self.interaction_timeout_ms
        }
    }
}

/// Tracks the connection lifecycle, pending requests, settled ids, and reconnect backoff.
#[derive(Debug, Clone)]
pub struct ConnectionTracker {
    limits: ConnectionLimits,
    state: ConnectionState,
    pending: BTreeMap<u64, PendingRequest>,
    completed: VecDeque<u64>,
    next_id: u64,
    registered_ids: u64,
    unknown_responses: usize,
    reconnect_attempts: u32,
}

impl Default for ConnectionTracker {
    fn default() -> Self {
        Self::with_limits(ConnectionLimits::default())
    }
}

impl ConnectionTracker {
    /// Creates a tracker with the protocol default limits and an empty pending set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a tracker with explicitly supplied limits.
    pub fn with_limits(limits: ConnectionLimits) -> Self {
        Self {
            limits,
            state: ConnectionState::Disconnected,
            pending: BTreeMap::new(),
            completed: VecDeque::new(),
            next_id: 1,
            registered_ids: 0,
            unknown_responses: 0,
            reconnect_attempts: 0,
        }
    }

    /// Returns the current lifecycle state.
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Returns the limits this tracker was built with.
    pub fn limits(&self) -> ConnectionLimits {
        self.limits
    }

    /// Applies a lifecycle transition, rejecting impossible orderings.
    pub fn transition_to(&mut self, next: ConnectionState) -> NaviResult<()> {
        if !self.state.can_transition_to(next) {
            let current = self.state;
            return Err(NaviError::Protocol(format!(
                "illegal connection transition {current:?} -> {next:?}"
            )));
        }
        if next == ConnectionState::Connected {
            self.reset_reconnect_backoff();
        }
        self.state = next;
        Ok(())
    }

    /// Returns the next correlation id and advances the counter by exactly one.
    pub fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// Registers an outbound request and returns the entry awaiting its response.
    ///
    /// A `None` deadline selects the method default from the connection limits.
    pub fn register_request(
        &mut self,
        method: impl Into<String>,
        now_ms: u64,
        timeout_ms: Option<u64>,
    ) -> NaviResult<PendingRequest> {
        if !self.state.accepts_requests() {
            let current = self.state;
            return Err(NaviError::Protocol(format!(
                "cannot register a request while {current:?}"
            )));
        }
        if self.registered_ids >= self.limits.max_request_ids_per_connection {
            return Err(NaviError::Protocol(
                "request id budget exhausted; open a new connection".to_owned(),
            ));
        }
        let method = method.into();
        let timeout_ms = timeout_ms.unwrap_or_else(|| self.limits.timeout_for(&method));
        let request = PendingRequest {
            id: self.next_request_id(),
            method,
            created_at_ms: now_ms,
            timeout_ms,
        };
        self.registered_ids = self.registered_ids.saturating_add(1);
        self.pending.insert(request.id, request.clone());
        Ok(request)
    }

    /// Correlates an inbound response with its pending request.
    ///
    /// A settled id is remembered, so a second answer reports a duplicate rather than being
    /// mistaken for a fresh request. Ids never registered are counted as protocol anomalies.
    pub fn resolve_response(&mut self, id: u64, response: Response) -> NaviResult<SettledResponse> {
        if let Some(request) = self.pending.remove(&id) {
            self.remember_completed(id);
            return Ok(SettledResponse { request, response });
        }
        if self.completed.contains(&id) {
            return Err(NaviError::Protocol(format!(
                "late response for already settled request {id}"
            )));
        }
        self.unknown_responses = self.unknown_responses.saturating_add(1);
        Err(NaviError::NotFound(format!(
            "response for unknown request id {id}"
        )))
    }

    /// Removes and returns every request whose deadline has been reached, ordered by id.
    pub fn expire_timed_out(&mut self, now_ms: u64) -> Vec<PendingRequest> {
        let due: Vec<u64> = self
            .pending
            .iter()
            .filter_map(|(id, request)| request.is_expired_at(now_ms).then_some(*id))
            .collect();
        let mut expired = Vec::with_capacity(due.len());
        for id in due {
            if let Some(request) = self.pending.remove(&id) {
                self.remember_completed(id);
                expired.push(request);
            }
        }
        expired
    }

    /// Returns the number of requests still awaiting a response.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Returns true when `id` is currently pending.
    pub fn contains(&self, id: u64) -> bool {
        self.pending.contains_key(&id)
    }

    /// Returns a reference to a pending request, if one exists.
    pub fn pending(&self, id: u64) -> Option<&PendingRequest> {
        self.pending.get(&id)
    }

    /// Returns how many responses named an id that was neither pending nor recently settled.
    pub fn unknown_responses(&self) -> usize {
        self.unknown_responses
    }

    /// Returns the deterministic capped exponential delay for a 1-based reconnect `attempt`.
    ///
    /// Attempt zero is treated as attempt one. The result is jitter-free so callers can schedule
    /// it against their own clock, and saturating so huge attempts cannot overflow.
    pub fn reconnect_backoff_ms(&self, attempt: u32) -> u64 {
        let shift = attempt.saturating_sub(1);
        let multiplier = if shift >= 64 {
            u64::MAX
        } else {
            1_u64 << shift
        };
        self.limits
            .reconnect_base_delay_ms
            .saturating_mul(multiplier)
            .min(self.limits.reconnect_max_delay_ms)
    }

    /// Advances the reconnect attempt counter and returns the next delay to wait.
    pub fn next_reconnect_delay_ms(&mut self) -> u64 {
        self.reconnect_attempts = self.reconnect_attempts.saturating_add(1);
        self.reconnect_backoff_ms(self.reconnect_attempts)
    }

    /// Returns the number of consecutive reconnect attempts since the last successful connection.
    pub fn reconnect_attempts(&self) -> u32 {
        self.reconnect_attempts
    }

    /// Clears the reconnect backoff after a successful connection.
    pub fn reset_reconnect_backoff(&mut self) {
        self.reconnect_attempts = 0;
    }

    /// Remembers a settled id in a bounded ring of recently completed requests.
    fn remember_completed(&mut self, id: u64) {
        self.completed.push_back(id);
        while self.completed.len() > self.limits.tracked_requests {
            self.completed.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn connected_tracker() -> ConnectionTracker {
        let mut tracker = ConnectionTracker::new();
        tracker
            .transition_to(ConnectionState::Connecting)
            .expect("enter connecting");
        tracker
            .transition_to(ConnectionState::Connected)
            .expect("enter connected");
        tracker
    }

    #[test]
    fn state_machine_follows_the_connection_lifecycle() {
        let mut tracker = ConnectionTracker::new();
        assert_eq!(tracker.state(), ConnectionState::Disconnected);
        assert!(!ConnectionState::Disconnected.accepts_requests());

        tracker
            .transition_to(ConnectionState::Connecting)
            .expect("disconnected to connecting");
        assert_eq!(tracker.state(), ConnectionState::Connecting);
        assert!(!ConnectionState::Connecting.accepts_requests());
        tracker
            .transition_to(ConnectionState::Connected)
            .expect("connecting to connected");
        assert!(
            ConnectionState::Connected.accepts_requests(),
            "a ready link serves requests"
        );
        tracker
            .transition_to(ConnectionState::Connected)
            .expect("idempotent connected");
        tracker
            .transition_to(ConnectionState::Closing)
            .expect("connected to closing");
        tracker
            .transition_to(ConnectionState::Disconnected)
            .expect("closing to disconnected");

        let mut fresh = ConnectionTracker::new();
        assert!(
            fresh.transition_to(ConnectionState::Connected).is_err(),
            "a handshake cannot be skipped"
        );
        let mut closing = connected_tracker();
        closing
            .transition_to(ConnectionState::Closing)
            .expect("connected to closing");
        assert!(
            closing.transition_to(ConnectionState::Connecting).is_err(),
            "a closing link must reach disconnected first"
        );
    }

    #[test]
    fn request_ids_start_at_one_and_increment_by_one() {
        let mut tracker = connected_tracker();
        let first = tracker
            .register_request("observe", 100, None)
            .expect("first request");
        let second = tracker
            .register_request("focus", 100, None)
            .expect("second request");
        let third = tracker
            .register_request("scroll", 100, Some(250))
            .expect("third request");

        assert_eq!(first.id, 1);
        assert_eq!(second.id, 2);
        assert_eq!(third.id, 3);
        assert_eq!(first.method, "observe");
        assert_eq!(first.created_at_ms, 100);
        assert_eq!(first.timeout_ms, 4_000, "observe uses the longer default");
        assert_eq!(second.timeout_ms, 2_000);
        assert_eq!(
            third.timeout_ms, 250,
            "an explicit deadline overrides the default"
        );
        assert_eq!(tracker.pending_count(), 3);
        assert_eq!(tracker.next_request_id(), 4);
    }

    #[test]
    fn registration_is_refused_while_not_connected_or_over_budget() {
        let mut disconnected = ConnectionTracker::new();
        assert!(
            disconnected.register_request("observe", 0, None).is_err(),
            "requests need a completed handshake"
        );

        let limits = ConnectionLimits {
            max_request_ids_per_connection: 2,
            ..ConnectionLimits::default()
        };
        let mut tracker = ConnectionTracker::with_limits(limits);
        tracker
            .transition_to(ConnectionState::Connecting)
            .expect("connecting");
        assert!(tracker.register_request("observe", 0, None).is_err());
        tracker
            .transition_to(ConnectionState::Connected)
            .expect("connected");
        assert!(tracker.register_request("observe", 0, None).is_ok());
        assert!(tracker.register_request("focus", 0, None).is_ok());
        let third = tracker.register_request("scroll", 0, None);
        assert!(third.is_err(), "the per-connection id budget is enforced");
    }

    #[test]
    fn resolving_a_response_settles_the_request_once() {
        let mut tracker = connected_tracker();
        let request = tracker
            .register_request("observe", 10, None)
            .expect("registered");
        let payload = json!({"op": "observe", "observationId": "o-1"});

        let settled = tracker
            .resolve_response(request.id, Response::Success(payload.clone()))
            .expect("settled");
        assert_eq!(settled.request, request);
        assert_eq!(settled.response, Response::Success(payload));
        assert_eq!(tracker.pending_count(), 0);
        assert!(!tracker.contains(request.id));

        let duplicate = tracker.resolve_response(request.id, Response::Success(json!({})));
        assert!(
            duplicate.is_err(),
            "a late duplicate is not a new correlation"
        );
        assert_eq!(tracker.unknown_responses(), 0);
    }

    #[test]
    fn unknown_and_error_responses_are_handled_without_unwrapping() {
        let mut tracker = connected_tracker();
        let unknown = tracker.resolve_response(9_999, Response::Success(json!({"ok": true})));
        assert!(matches!(unknown, Err(NaviError::NotFound(_))));
        assert_eq!(tracker.unknown_responses(), 1);

        let request = tracker
            .register_request("focus", 0, Some(50))
            .expect("registered");
        let error = NaviError::Timeout("no confirmation before the deadline".to_owned());
        let settled = tracker
            .resolve_response(request.id, Response::Error(error.clone()))
            .expect("settled");
        assert_eq!(settled.response, Response::Error(error));
        assert_eq!(
            tracker.unknown_responses(),
            1,
            "a real result does not count as unknown"
        );
    }

    #[test]
    fn expiry_is_inclusive_saturating_and_ordered() {
        let mut tracker = connected_tracker();
        let observe = tracker
            .register_request("observe", 0, Some(10))
            .expect("observe");
        let focus = tracker
            .register_request("focus", 5, Some(5))
            .expect("focus");
        let live = tracker
            .register_request("scroll", 5, Some(20))
            .expect("scroll");

        assert!(tracker.expire_timed_out(9).is_empty());
        let expired = tracker.expire_timed_out(10);
        assert_eq!(expired, vec![observe, focus]);
        assert!(
            tracker.expire_timed_out(10).is_empty(),
            "expiry removes entries"
        );
        assert_eq!(tracker.pending_count(), 1);
        assert!(tracker.contains(live.id));
        tracker
            .resolve_response(live.id, Response::Success(json!({})))
            .expect("settle the live request");

        let overflow = tracker
            .register_request("observe", u64::MAX, Some(100))
            .expect("overflowing deadline");
        assert_eq!(overflow.deadline_ms(), u64::MAX);
        assert!(!overflow.is_expired_at(u64::MAX - 1));
        assert!(tracker.expire_timed_out(u64::MAX - 1).is_empty());
        assert_eq!(tracker.expire_timed_out(u64::MAX), vec![overflow]);
    }

    #[test]
    fn settled_id_memory_is_bounded() {
        let limits = ConnectionLimits {
            tracked_requests: 2,
            ..ConnectionLimits::default()
        };
        let mut tracker = ConnectionTracker::with_limits(limits);
        tracker
            .transition_to(ConnectionState::Connecting)
            .expect("connecting");
        tracker
            .transition_to(ConnectionState::Connected)
            .expect("connected");
        let ids: Vec<u64> = (0..3)
            .map(|_| {
                tracker
                    .register_request("observe", 0, None)
                    .expect("registered")
                    .id
            })
            .collect();
        for id in &ids {
            tracker
                .resolve_response(*id, Response::Success(json!({})))
                .expect("settled");
        }

        assert!(
            tracker
                .resolve_response(ids[2], Response::Success(json!({})))
                .is_err(),
            "the last settled id is still remembered"
        );
        assert!(
            tracker
                .resolve_response(ids[0], Response::Success(json!({})))
                .is_err(),
            "the oldest settled id has been evicted"
        );
        assert_eq!(
            tracker.unknown_responses(),
            1,
            "eviction is reported as unknown"
        );
    }

    #[test]
    fn reconnect_backoff_grows_exponentially_and_caps() {
        let tracker = ConnectionTracker::new();
        assert_eq!(tracker.reconnect_backoff_ms(0), 100);
        assert_eq!(tracker.reconnect_backoff_ms(1), 100);
        assert_eq!(tracker.reconnect_backoff_ms(2), 200);
        assert_eq!(tracker.reconnect_backoff_ms(3), 400);
        assert_eq!(tracker.reconnect_backoff_ms(8), 12_800);
        assert_eq!(tracker.reconnect_backoff_ms(9), 25_600);
        assert_eq!(tracker.reconnect_backoff_ms(10), 30_000);
        assert_eq!(tracker.reconnect_backoff_ms(u32::MAX), 30_000);

        let zero_base = ConnectionTracker::with_limits(ConnectionLimits {
            reconnect_base_delay_ms: 0,
            reconnect_max_delay_ms: u64::MAX,
            ..ConnectionLimits::default()
        });
        assert_eq!(zero_base.reconnect_backoff_ms(u32::MAX), 0);
    }

    #[test]
    fn reconnect_backoff_resets_after_a_successful_connection() {
        let mut tracker = ConnectionTracker::new();
        assert_eq!(tracker.next_reconnect_delay_ms(), 100);
        assert_eq!(tracker.next_reconnect_delay_ms(), 200);
        assert_eq!(tracker.reconnect_attempts(), 2);

        tracker
            .transition_to(ConnectionState::Connecting)
            .expect("connecting");
        tracker
            .transition_to(ConnectionState::Connected)
            .expect("connected");
        assert_eq!(tracker.reconnect_attempts(), 0);
        assert_eq!(tracker.next_reconnect_delay_ms(), 100);

        tracker.reset_reconnect_backoff();
        assert_eq!(tracker.reconnect_attempts(), 0);
        assert_eq!(
            tracker.limits().handshake_deadline_ms(1_000),
            6_000,
            "handshake timeout is applied from the start of the handshake"
        );
    }
}
