//! `Consensus` is a state-machine (not to be confused with the `StateMachine` trait) which
//! implements the logic of the Raft Protocol. A `Consensus` receives events from the local
//! `Server`. The set of possible events is specified by the Raft Protocol:
//!
//! ```text
//! Event = AppendEntriesRequest | AppendEntriesResponse
//!       | RequestVoteRequest   | RequestVoteResponse
//!       | ElectionTimeout      | HeartbeatTimeout
//!       | ClientProposal       | ClientQuery
//! ```
//!
//! In response to an event, the `Consensus` may mutate its own state, apply a command to the local
//! `StateMachine`, or return an event to be sent to one or more remote peers or clients.

use std::{cmp, fmt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;

use capnp::message::{Builder, Allocator, ReaderOptions, HeapAllocator, Reader};
use rand::{self, Rng};
use capnp::serialize::{self, OwnedSegments};
use std::io::Cursor;

use {LogId, LogIndex, Term, ServerId, ClientId, messages, TransactionId};
use messages_capnp::{append_entries_request, append_entries_response, client_request,
                     proposal_request, query_request, message, request_vote_request,
                     request_vote_response};
use state::{ConsensusState, LeaderState, CandidateState, FollowerState};
use state_machine::StateMachine;
use transaction::TransactionManager;
use persistent_log::Log;
use mio::Timeout as TimeoutHandle;

use std::sync::{Arc, RwLock};

use transaction;

const ELECTION_MIN: u64 = 5000;
const ELECTION_MAX: u64 = 10000;
const HEARTBEAT_DURATION: u64 = 2000;

/// Consensus timeout types.
// TODO Remove LogId, because not neccessary
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ConsensusTimeout {
    // An election timeout. Randomized value.
    Election(LogId),
    // A heartbeat timeout. Stable value.
    Heartbeat(ServerId, LogId),
}

impl ConsensusTimeout {
    /// Returns the timeout period in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        match *self {
            ConsensusTimeout::Election(..) => {
                rand::thread_rng().gen_range::<u64>(ELECTION_MIN, ELECTION_MAX)
            }
            ConsensusTimeout::Heartbeat(..) => HEARTBEAT_DURATION,
        }
    }
}

/// A set of actions for the `Server` to carry out asyncronously in response to applying an event
/// to a `Consensus` state machine.
pub struct Actions {
    /// Messages to be sent to peers.
    pub peer_messages: Vec<(ServerId, Rc<Builder<HeapAllocator>>)>,
    /// Messages to be send to clients.
    pub client_messages: Vec<(ClientId, Rc<Builder<HeapAllocator>>)>,
    /// Whether to clear existing consensus timeouts.
    pub clear_timeouts: Vec<LogId>,
    /// Any new timeouts to create.
    pub timeouts: Vec<ConsensusTimeout>,
    /// Whether to clear outbound peer message queues.
    pub clear_peer_messages: bool,
    /// Messages which are in queue because there is a transaction active
    pub transaction_queue: Vec<(LogId, ClientId, Builder<HeapAllocator>)>,
    /// Messages to be send to all peers in the cluster
    pub peer_messages_broadcast: Vec<Rc<Builder<HeapAllocator>>>,
}

impl fmt::Debug for Actions {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let peer_messages: Vec<ServerId> = self.peer_messages
            .iter()
            .map(|peer_message| peer_message.0)
            .collect();
        let client_messages: Vec<ClientId> = self.client_messages
            .iter()
            .map(|client_message| client_message.0)
            .collect();
        let transaction_messages: Vec<ClientId> = self.transaction_queue
            .iter()
            .map(|transaction_message| transaction_message.1)
            .collect();
        write!(fmt,
               "Actions {{ peer_messages: {:?}, client_messages: {:?}, clear_timeouts: {:?}, \
                timeouts: {:?}, clear_peer_messages: {}, transaction_queue: {:?} }}",
               peer_messages,
               client_messages,
               self.clear_timeouts,
               self.timeouts,
               self.clear_peer_messages,
               transaction_messages)
    }
}

impl Actions {
    /// Creates an empty `Actions` set.
    pub fn new() -> Actions {
        Actions {
            peer_messages: vec![],
            client_messages: vec![],
            clear_timeouts: Vec::with_capacity(1),
            timeouts: vec![],
            clear_peer_messages: false,
            transaction_queue: vec![],
            peer_messages_broadcast: vec![],
        }
    }
}

/// An instance of a Raft state machine. The Consensus controls a client state machine, to which it
/// applies entries in a globally consistent order.
pub struct Consensus<L, M> {
    /// The ID of this consensus instance.
    id: ServerId,
    /// The IDs of peers in the consensus group.
    pub peers: HashMap<ServerId, SocketAddr>,

    /// The persistent log.
    pub log: L,
    /// The client state machine to which client commands are applied.
    pub state_machine: Arc<RwLock<M>>,

    /// Index of the latest entry known to be committed.
    commit_index: LogIndex,
    /// Index of the latest entry applied to the state machine.
    last_applied: LogIndex,

    /// The current state of the `Consensus` (`Leader`, `Candidate`, or `Follower`).
    pub state: ConsensusState,
    /// State necessary while a `Leader`. Should not be used otherwise.
    pub leader_state: Arc<RwLock<LeaderState>>,
    /// State necessary while a `Candidate`. Should not be used otherwise.
    pub candidate_state: Arc<RwLock<CandidateState>>,
    /// State necessary while a `Follower`. Should not be used otherwise.
    pub follower_state: Arc<RwLock<FollowerState>>,
    pub transaction: TransactionManager,
    /// The ID of this consensus instance for the log_manager
    lid: LogId,
    /// Currently registered consensus timeouts.
    pub consensus_timeouts: HashMap<ConsensusTimeout, TimeoutHandle>,
}

impl<L, M> Consensus<L, M>
    where L: Log,
          M: StateMachine
{
    /// Creates a `Consensus`.
    pub fn new(id: ServerId,
               lid: LogId,
               peers: HashMap<ServerId, SocketAddr>,
               log: L,
               state_machine: M)
               -> Consensus<L, M> {
        let leader_state = LeaderState::new(log.latest_log_index().unwrap(),
                                            &peers.keys().cloned().collect());
        Consensus {
            id: id,
            peers: peers,
            log: log,
            state_machine: Arc::new(RwLock::new(state_machine)),
            commit_index: LogIndex(0),
            last_applied: LogIndex(0),
            state: ConsensusState::Follower,
            leader_state: Arc::new(RwLock::new(leader_state)),
            candidate_state: Arc::new(RwLock::new(CandidateState::new())),
            follower_state: Arc::new(RwLock::new(FollowerState::new())),
            transaction: TransactionManager::new(),
            lid: lid,
            consensus_timeouts: HashMap::new(),
        }
    }

    /// Returns the consenus peers.
    pub fn peers(&self) -> &HashMap<ServerId, SocketAddr> {
        &self.peers
    }

    /// If a transaction is inactive, method processes client messages
    pub fn handle_queue(&mut self,
                        requests_in_queue: &mut Vec<(ClientId, Builder<HeapAllocator>)>,
                        actions: &mut Actions)
                        -> Result<(), ()> {
        if !self.transaction.is_active {
            for (client, builder) in requests_in_queue.pop() {
                self.apply_client_message(client,
                                          &Self::into_reader(&builder)
                                              .get_root::<client_request::Reader>()
                                              .unwrap(),
                                          actions);
            }

            Ok(())
        } else {
            Ok(())
        }
    }

    /// Applies a peer message to the consensus state machine.
    pub fn apply_peer_message(&mut self,
                              from: ServerId,
                              message: &message::Reader,
                              actions: &mut Actions) {
        push_log_scope!("{:?}", self);
        let reader = message.which().unwrap();
        match reader {
            message::Which::AppendEntriesRequest(Ok(request)) => {
                self.append_entries_request(from, request, actions)
            }
            message::Which::AppendEntriesResponse(Ok(response)) => {
                self.append_entries_response(from, response, actions)
            }
            message::Which::RequestVoteRequest(Ok(request)) => {
                self.request_vote_request(from, request, actions)
            }
            message::Which::RequestVoteResponse(Ok(response)) => {
                self.request_vote_response(from, response, actions)
            }
            message::Which::TransactionBegin(Ok(response)) => {
                // TODO do not panic if invalid
                self.transaction_begin(from,
                                       TransactionId::from_bytes(response.get_session().unwrap())
                                           .expect("Invalid TransactionId"),
                                       actions)
            }
            message::Which::TransactionCommit(Ok(message)) => {
                // TODO do not panic if invalid
                self.transaction_commit(from,
                                        TransactionId::from_bytes(message.get_session().unwrap())
                                            .expect("Invalid TransactionId"),
                                        actions)
            }
            message::Which::TransactionRollback(Ok(message)) => {
                scoped_debug!("Rollback");

                // TODO do not panic if invalid
                self.transaction_rollback(from,
                                          TransactionId::from_bytes(message.get_session().unwrap())
                                              .expect("Invalid TransactionId"),
                                          actions)

            }
            _ => panic!("cannot handle message"),
        }
    }

    /// Applies a client message to the consensus state machine.
    pub fn apply_client_message(&mut self,
                                from: ClientId,
                                message: &client_request::Reader,
                                actions: &mut Actions) {

        push_log_scope!("{:?}", self);
        let reader = message.which().unwrap();

        match reader {
            client_request::Which::Proposal(Ok(request)) => {
                if self.is_leader() {
                    let session = TransactionId::from_bytes(request.get_session().unwrap())
                        .expect("Invalid TransactionId");

                    if self.transaction.is_active &&
                       !self.transaction
                        .compare(session) {

                        let entry = request.get_entry().unwrap();

                        let message = messages::proposal_request(session, entry, self.lid);

                        actions.transaction_queue.push((self.lid, from, message));

                        self.transaction.count_up();
                    } else {
                        self.proposal_request(from, request, actions)
                    }

                } else {
                    self.proposal_request(from, request, actions)
                }
            }
            client_request::Which::Query(Ok(query)) => {
                if self.transaction.is_active {
                    let query = query.get_query().unwrap();
                    let message = messages::query_request(query, &self.lid);

                    actions.transaction_queue.push((self.lid, from, message));
                } else {
                    self.query_request(from, query, actions);
                }
            }
            client_request::Which::TransactionBegin(Ok(request)) => {
                self.client_transaction_begin(from,
                                              TransactionId::from_bytes(request.get_session()
                                                      .unwrap())
                                                  .expect("Transaction invalid"),
                                              actions);
            }
            client_request::Which::TransactionCommit(Ok(_)) => {
                self.client_transaction_commit(from, actions);
            }
            client_request::Which::TransactionRollback(Ok(_)) => {
                self.client_transaction_rollback(from, actions);

            }
            _ => panic!("cannot handle message"),
        }
    }

    /// Applies a timeout's actions to the `Consensus`.
    pub fn apply_timeout(&mut self, timeout: ConsensusTimeout, actions: &mut Actions) {
        push_log_scope!("{:?}", self);
        match timeout {
            ConsensusTimeout::Election(..) => self.election_timeout(actions),
            ConsensusTimeout::Heartbeat(peer, ..) => self.heartbeat_timeout(peer, actions),
        }
    }

    /// Adds new peer to `peers`
    ///
    /// # Arguments
    /// * `peer_id` - The ID of the new peer
    /// * `peer_addr` - The socketaddress of the new peer
    pub fn add_peer(&mut self, peer_id: ServerId, peer_addr: SocketAddr) {
        assert_eq!(self.peers.insert(peer_id, peer_addr), None);

        let mut lock = self.leader_state.write().unwrap();

        lock.add_peer(peer_id);
    }

    /// Notifies the consensus state machine that a new connection to the peer exists, and
    /// in-flight messages may have been lost.
    pub fn peer_connection_reset(&mut self,
                                 peer: ServerId,
                                 addr: SocketAddr,
                                 actions: &mut Actions) {
        push_log_scope!("{:?}", self);

        self.peers.insert(peer, addr);

        match self.state {
            ConsensusState::Leader => {
                // Send any outstanding entries to the peer, or an empty heartbeat if there are no
                // outstanding entries.
                let mut leader_state = self.leader_state.write().unwrap();
                let from_index = leader_state.next_index(&peer);
                let until_index = self.latest_log_index() + 1;

                let prev_log_index = from_index - 1;
                let prev_log_term = if prev_log_index == LogIndex::from(0) {
                    Term::from(0)
                } else {
                    self.log.entry(prev_log_index).unwrap().0
                };

                let entries = self.log.entries(from_index, until_index).unwrap();
                let message = messages::append_entries_request(self.current_term(),
                                                               prev_log_index,
                                                               prev_log_term,
                                                               &entries,
                                                               self.commit_index,
                                                               &self.lid);

                leader_state.set_next_index(peer, until_index);
                actions.peer_messages.push((peer, message));
            }
            ConsensusState::Candidate => {
                // Resend the request vote request if a response has not yet been receieved.
                if self.candidate_state.read().unwrap().peer_voted(peer) {
                    return;
                }
                let current_term = self.current_term();
                let latest_index = self.latest_log_index();
                let latest_term = self.log.latest_log_term().unwrap();

                let message = messages::request_vote_request(current_term,
                                                             latest_index,
                                                             latest_term,
                                                             &self.lid);
                actions.peer_messages.push((peer, message));
            }
            ConsensusState::Follower => {
                // No message is necessary; if the peer is a leader or candidate they will send a
                // message.
            }
        }
    }

    /// Apply an append entries request to the consensus state machine.
    fn append_entries_request(&mut self,
                              from: ServerId,
                              request: append_entries_request::Reader,
                              actions: &mut Actions) {
        scoped_trace!("AppendEntriesRequest from peer {}", &from);

        let leader_term = Term(request.get_term());
        let current_term = self.current_term();

        if leader_term < current_term {
            let message = messages::append_entries_response_stale_term(current_term, &self.lid);
            actions.peer_messages.push((from, message));
            return;
        }

        match self.state {
            ConsensusState::Follower => {
                let message = {
                    if current_term < leader_term {
                        self.log.set_current_term(leader_term).unwrap();
                        self.follower_state.write().unwrap().set_leader(from);
                    }

                    let leader_prev_log_index = LogIndex(request.get_prev_log_index());
                    let leader_prev_log_term = Term(request.get_prev_log_term());

                    let latest_log_index = self.latest_log_index();

                    if latest_log_index < leader_prev_log_index {
                        // If the previous entries index was not the same we'd leave a gap! Reply failure.
                        scoped_debug!("AppendEntriesRequest: inconsistent previous log index: \
                                      leader: {}, local: {}",
                                      leader_prev_log_index,
                                      latest_log_index);

                        messages::append_entries_response_inconsistent_prev_entry(
                            self.current_term(), leader_prev_log_index,&self.lid)
                    } else {
                        let existing_term = if leader_prev_log_index == LogIndex::from(0) {
                            Term::from(0)
                        } else {
                            self.log.entry(leader_prev_log_index).unwrap().0
                        };

                        if existing_term != leader_prev_log_term {
                            scoped_debug!("AppendEntriesRequest: inconsistent previous log term: \
                                          leader term: {}, local term: {}",
                                          leader_prev_log_term,
                                          existing_term);
                            // If an existing entry conflicts with a new one (same index but different terms),
                            // delete the existing entry and all that follow it
                            messages::append_entries_response_inconsistent_prev_entry(self.current_term(),
                                leader_prev_log_index,&self.lid)
                        } else {
                            if let Ok(entries) = request.get_entries() {
                                let num_entries: u32 = entries.len();
                                let new_latest_log_index = leader_prev_log_index +
                                                           num_entries as u64;

                                if new_latest_log_index <
                                   self.follower_state.read().unwrap().min_index {
                                    // Stale entry; ignore. This guards against overwriting a
                                    // possibly committed part of the log if messages get
                                    // rearranged; see ktoso/akka-raft#66.
                                    return;
                                }
                                scoped_debug!("AppendEntriesRequest: {} entries from leader: {}",
                                              num_entries,
                                              from);

                                let entries_vec: Vec<(Term, &[u8])> = entries.iter()
                                    .map(|entry| {
                                        (Term::from(entry.get_term()),
                                         entry.get_data().unwrap_or(b""))
                                    })
                                    .collect();

                                self.log
                                    .append_entries(leader_prev_log_index + 1, &entries_vec)
                                    .unwrap();
                                self.follower_state.write().unwrap().min_index =
                                    new_latest_log_index;
                                // We are matching the leader's log up to and including `new_latest_log_index`.
                                self.commit_index =
                                    cmp::min(LogIndex::from(request.get_leader_commit()),
                                             new_latest_log_index);
                                self.apply_commits();

                            } else {
                                panic!("AppendEntriesRequest: no entry list")
                            }

                            messages::append_entries_response_success(self.current_term(),
                                                                      self.log
                                                                          .latest_log_index()
                                                                          .unwrap(),
                                                                      &self.lid)
                        }
                    }
                };

                actions.clear_timeouts.push(self.lid);
                actions.timeouts.push(ConsensusTimeout::Election(self.lid));
                actions.peer_messages.push((from, message.clone()));
            }
            ConsensusState::Candidate => {
                // recognize the new leader, return to follower state, and apply the entries
                scoped_info!("received AppendEntriesRequest from Consensus {{ id: {}, term: {} \
                              }} with newer term; transitioning to Follower",
                             from,
                             leader_term);
                self.transition_to_follower(leader_term, from, actions);
                return self.append_entries_request(from, request, actions);
            }
            ConsensusState::Leader => {
                if leader_term == current_term {
                    // The single leader-per-term invariant is broken; there is a bug in the Raft
                    // implementation.
                    panic!("{:?}: peer leader {} with matching term {:?} detected.",
                           self,
                           from,
                           current_term);
                }

                // recognize the new leader, return to follower state, and apply the entries
                scoped_info!("received AppendEntriesRequest from Consensus {{ id: {}, term: {} \
                              }} with newer term; transitioning to Follower",
                             from,
                             leader_term);
                self.transition_to_follower(leader_term, from, actions);
                return self.append_entries_request(from, request, actions);
            }
        }
    }

    /// Apply an append entries response to the consensus state machine.
    ///
    /// The provided message may be initialized with a new AppendEntries request to send back to
    /// the follower in the case that the follower's log is behind.
    fn append_entries_response(&mut self,
                               from: ServerId,
                               response: append_entries_response::Reader,
                               actions: &mut Actions) {
        let local_term = self.current_term();
        let responder_term = Term::from(response.get_term());
        let local_latest_log_index = self.latest_log_index();

        if local_term < responder_term {
            // Responder has a higher term number. Relinquish leader position (if it is held), and
            // return to follower status.

            // The responder is not necessarily the leader, but it is somewhat likely, so we will
            // use it as the leader hint.
            scoped_info!("AppendEntriesResponse from peer {} with newer term: {}; \
                         transitioning to Follower",
                         from,
                         responder_term);
            self.transition_to_follower(responder_term, from, actions);
            return;
        } else if local_term > responder_term {
            scoped_debug!("AppendEntriesResponse from peer {} with a different term: {}",
                          from,
                          responder_term);
            // Responder is responding to an AppendEntries request from a different term. Ignore
            // the response.
            return;
        }

        match response.which() {
            Ok(append_entries_response::Which::Success(follower_latest_log_index)) => {
                scoped_trace!("AppendEntriesResponse from peer {}: success", from);
                scoped_assert!(self.is_leader());
                let follower_latest_log_index = LogIndex::from(follower_latest_log_index);
                // scoped_assert!(follower_latest_log_index <= local_latest_log_index);
                scoped_debug!("Follower_log_index {}", follower_latest_log_index);
                self.leader_state.write().unwrap().set_match_index(from, follower_latest_log_index);
                self.advance_commit_index(actions);
            }
            Ok(append_entries_response::Which::InconsistentPrevEntry(next_index)) => {
                scoped_assert!(self.is_leader());
                scoped_debug!("AppendEntriesResponse from peer {}: \
                              inconsistent previous entry index: {}",
                              from,
                              next_index);
                self.leader_state.write().unwrap().set_next_index(from, LogIndex::from(next_index));
            }
            Ok(append_entries_response::Which::StaleTerm(..)) => {
                // The peer is reporting a stale term, but the term number matches the local term.
                // Ignore the response, since it is to a message from a prior term, and this server
                // has already transitioned to the new term.
                scoped_debug!("AppendEntriesResponse from peer {}: stale term (outdated)",
                              from);
                return;
            }
            Ok(append_entries_response::Which::InternalError(error_result)) => {
                let error = error_result.unwrap_or("[unable to decode internal error]");
                scoped_warn!("AppendEntriesResponse from peer {}: internal error: {}",
                             from,
                             error);
            }
            Err(error) => {
                scoped_warn!("AppendEntriesResponse from peer {}: unable to deserialize \
                              response: {}",
                             from,
                             error);
            }
        }

        let next_index = self.leader_state.write().unwrap().next_index(&from);
        if next_index <= local_latest_log_index {
            // If the peer is behind, send it entries to catch up.
            scoped_debug!("AppendEntriesResponse: peer {} is missing at least {} entries; \
                          sending missing entries",
                          from,
                          (local_latest_log_index + 1 - next_index.0).0);
            let prev_log_index = next_index - 1;
            let prev_log_term = if prev_log_index == LogIndex(0) {
                Term(0)
            } else {
                self.log.entry(prev_log_index).unwrap().0
            };

            let from_index = next_index;
            let until_index = local_latest_log_index + 1;

            let entries = self.log
                .entries(LogIndex::from(from_index), LogIndex::from(until_index))
                .unwrap();

            let message = messages::append_entries_request(local_term,
                                                           prev_log_index,
                                                           prev_log_term,
                                                           &entries,
                                                           self.commit_index,
                                                           &self.lid);

            self.leader_state.write().unwrap().set_next_index(from, local_latest_log_index + 1);
            actions.peer_messages.push((from, message));
        } else {
            // If the peer is caught up, set a heartbeat timeout.
            scoped_trace!("AppendEntriesResponse: scheduling heartbeat for peer {}",
                          from);
            let timeout = ConsensusTimeout::Heartbeat(from, self.lid);
            actions.timeouts.push(timeout);
        }
    }

    /// Applies a peer request vote request to the consensus state machine.
    fn request_vote_request(&mut self,
                            candidate: ServerId,
                            request: request_vote_request::Reader,
                            actions: &mut Actions) {

        let candidate_term = Term(request.get_term());
        let candidate_log_term = Term(request.get_last_log_term());
        let candidate_log_index = LogIndex(request.get_last_log_index());
        scoped_debug!("RequestVoteRequest from Consensus {{ id: {}, term: {}, latest_log_term: \
                       {}, latest_log_index: {} }}",
                      &candidate,
                      candidate_term,
                      candidate_log_term,
                      candidate_log_index);
        let local_term = self.current_term();

        let new_local_term = if candidate_term > local_term {
            scoped_info!("received RequestVoteRequest from Consensus {{ id: {}, term: {} }} \
                         with newer term; transitioning to Follower",
                         candidate,
                         candidate_term);
            self.transition_to_follower(candidate_term, candidate, actions);
            candidate_term
        } else {
            local_term
        };

        let message = if candidate_term < local_term {
            messages::request_vote_response_stale_term(new_local_term, &self.lid)
        } else if candidate_log_term < self.latest_log_term() ||
                                candidate_log_index < self.latest_log_index() {
            messages::request_vote_response_inconsistent_log(new_local_term, &self.lid)
        } else {
            match self.log.voted_for().unwrap() {
                None => {
                    self.log.set_voted_for(Some(candidate)).unwrap();
                    messages::request_vote_response_granted(new_local_term, &self.lid)
                }
                Some(voted_for) if voted_for == candidate => {
                    messages::request_vote_response_granted(new_local_term, &self.lid)
                }
                _ => messages::request_vote_response_already_voted(new_local_term, &self.lid),
            }
        };
        actions.peer_messages.push((candidate, message));
    }

    /// Applies a request vote response to the consensus state machine.
    fn request_vote_response(&mut self,
                             from: ServerId,
                             response: request_vote_response::Reader,
                             actions: &mut Actions) {

        scoped_debug!("RequestVoteResponse from peer {}", from);

        let local_term = self.current_term();
        let voter_term = Term::from(response.get_term());

        let majority = self.majority();
        if local_term < voter_term {
            // Responder has a higher term number. The election is compromised; abandon it and
            // revert to follower state with the updated term number. Any further responses we
            // receive from this election term will be ignored because the term will be outdated.

            // The responder is not necessarily the leader, but it is somewhat likely, so we will
            // use it as the leader hint.
            scoped_info!("received RequestVoteResponse from Consensus {{ id: {}, term: {} }} \
                         with newer term; transitioning to Follower",
                         from,
                         voter_term);
            self.transition_to_follower(voter_term, from, actions);
        } else if local_term > voter_term {
            // Ignore this message; it came from a previous election cycle.
        } else if self.is_candidate() {
            // A vote was received!
            if let Ok(request_vote_response::Granted(_)) = response.which() {
                {
                    self.candidate_state.write().unwrap().record_vote(from);
                }
                if self.candidate_state.read().unwrap().count_votes() >= majority {
                    scoped_info!("election for term {} won; transitioning to Leader",
                                 local_term);
                    self.transition_to_leader(actions);
                }
            }
        };
    }

    /// Applies a client proposal to the consensus state machine.
    pub fn proposal_request(&mut self,
                            from: ClientId,
                            request: proposal_request::Reader,
                            actions: &mut Actions) {

        if self.is_candidate() ||
           (self.is_follower() && self.follower_state.read().unwrap().leader.is_none()) {
            actions.client_messages
                .push((from, messages::command_response_unknown_leader(self.lid)));
        } else if self.is_follower() {
            let message = messages::command_response_not_leader(&self.peers[&self.follower_state
                                                                    .read()
                                                                    .unwrap()
                                                                    .leader
                                                                    .unwrap()],
                                                                self.lid);
            actions.client_messages.push((from, message));
        } else if let Ok(entry) = request.get_entry() {
            let prev_log_index = self.latest_log_index();
            let prev_log_term = self.latest_log_term();
            let term = self.current_term();
            let log_index = prev_log_index + 1;
            self.log.append_entries(log_index, &[(term, entry)]).unwrap();
            self.leader_state.write().unwrap().proposals.push_back((from, log_index));
            if self.peers.is_empty() {
                scoped_debug!("ProposalRequest from client {}: entry {}", from, log_index);
                self.advance_commit_index(actions);
            } else {
                scoped_debug!("ProposalRequest from client {}: sending entry {} to peers",
                              from,
                              log_index);
                let message = messages::append_entries_request(term,
                                                               prev_log_index,
                                                               prev_log_term,
                                                               &[(term, entry)],
                                                               self.commit_index,
                                                               &self.lid);
                let mut leader_state = self.leader_state.write().unwrap();
                for &peer in self.peers.keys() {
                    if leader_state.next_index(&peer) == log_index {
                        actions.peer_messages.push((peer, message.clone()));
                        leader_state.set_next_index(peer, log_index + 1);
                    }
                }
            }
        } else {
            panic!("ProposalRequest: no entry given")
        }
    }

    /// Starts new transaction
    fn transaction_begin(&mut self, _: ServerId, session: TransactionId, _: &mut Actions) {
        if !self.is_leader() {
            if !self.transaction.is_active {
                self.transaction.begin(session,
                                       self.commit_index,
                                       self.last_applied,
                                       Some(self.follower_state.read().unwrap().min_index));
            } else {
                scoped_warn!("A transaction is already running");
            }
        }
    }

    /// Ends the current transaction successfully
    fn transaction_commit(&mut self, _: ServerId, session: TransactionId, _: &mut Actions) {
        if self.transaction.is_active {
            assert_eq!(self.transaction.session.expect("No TransactionSession defined"),
                       session);
            self.transaction.end();
        } else {
            scoped_warn!("Received TransactionCommit but no transaction is currently running");
        }

    }

    /// Ends the current transaction and rolls all messages in this transaction back
    fn transaction_rollback(&mut self, _: ServerId, _: TransactionId, _: &mut Actions) {
        if self.transaction.is_active {
            let (commit_index, last_applied, follower_state_min) = self.transaction
                .rollback()
                .unwrap();
            self.follower_state.write().unwrap().min_index = follower_state_min.unwrap();
            self.commit_index = commit_index;
            self.last_applied = last_applied;

            {
                let entries_failed = self.log.rollback(commit_index).unwrap();

                for &(_, ref command) in entries_failed.iter().rev() {
                    self.state_machine.write().unwrap().revert(command.as_slice());
                }
            }

            self.log.truncate(commit_index).unwrap();
            self.state_machine.write().unwrap().rollback();
        } else {
            scoped_warn!("Cannot rollback; no transaction running");
        }
    }

    /// Client starts new transaction
    fn client_transaction_begin(&mut self,
                                from: ClientId,
                                session: TransactionId,
                                actions: &mut Actions) {
        if self.is_leader() {
            if !self.transaction.is_active {
                self.transaction
                    .begin(session, self.commit_index, self.last_applied, None)
                    .unwrap();
                self.transaction.broadcast_begin(self.lid, actions);

                let message = messages::command_transaction_success(&session.as_bytes(), self.lid);

                actions.client_messages.push((from, message));
            } else {
                let message =
                    messages::command_transaction_failure(transaction::TransactionError::NotActive,
                                                          self.lid);

                actions.client_messages.push((from, message));
            }
        } else {
            let message = messages::command_response_not_leader(&self.peers[&self.follower_state
                                                                    .read()
                                                                    .unwrap()
                                                                    .leader
                                                                    .unwrap()],
                                                                self.lid);
            actions.client_messages.push((from, message));
        }
    }

    /// Client ends transaction
    fn client_transaction_commit(&mut self, from: ClientId, actions: &mut Actions) {
        if self.is_leader() {
            if self.transaction.is_active {
                self.transaction.broadcast_end(self.lid, actions);
                self.transaction.end().unwrap();

                let message =
                    messages::command_transaction_success(b"Transaction has been stopped",
                                                          self.lid);

                actions.client_messages.push((from, message));
            } else {
                let message =
                    messages::command_transaction_failure(transaction::TransactionError::AlreadyActive,
                                                          self.lid);
                actions.client_messages.push((from, message));

            }

        } else {
            let message = messages::command_response_not_leader(&self.peers[&self.follower_state
                                                                    .read()
                                                                    .unwrap()
                                                                    .leader
                                                                    .unwrap()],
                                                                self.lid);
            actions.client_messages.push((from, message));
        }
    }

    /// Client rollback transaction
    fn client_transaction_rollback(&mut self, from: ClientId, actions: &mut Actions) {
        if self.is_leader() {
            if self.transaction.is_active {
                self.transaction.broadcast_rollback(self.lid, actions);
                let (commit_index, last_applied, _) = self.transaction.rollback().unwrap();
                self.commit_index = commit_index;
                self.last_applied = last_applied;
                self.log.rollback(commit_index).expect("Transaction rollback failed");

                let message = messages::command_transaction_success(b"", self.lid);

                let mut leader_state = self.leader_state.write().unwrap();
                for &peer in self.peers.keys() {
                    leader_state.set_next_index(peer, commit_index + 1);
                }

                {
                    let entries_failed = self.log.rollback(commit_index).unwrap();

                    for &(_, ref command) in entries_failed.iter().rev() {
                        self.state_machine.write().unwrap().revert(command.as_slice());
                    }
                }

                self.log.truncate(commit_index).unwrap();
                self.state_machine.write().unwrap().rollback();

                actions.client_messages.push((from, message));
            } else {
                let message =
                    messages::command_transaction_failure(transaction::TransactionError::AlreadyActive,
                                                          self.lid);
                actions.client_messages.push((from, message));

            }
        } else {
            let message = messages::command_response_not_leader(&self.peers[&self.follower_state
                                                                    .read()
                                                                    .unwrap()
                                                                    .leader
                                                                    .unwrap()],
                                                                self.lid);
            actions.client_messages.push((from, message));
        }
    }


    /// Applies a client query to the state machine.
    pub fn query_request(&mut self,
                         from: ClientId,
                         request: query_request::Reader,
                         actions: &mut Actions) {

        if self.is_candidate() ||
           (self.is_follower() && self.follower_state.read().unwrap().leader.is_none()) {
            actions.client_messages
                .push((from, messages::command_response_unknown_leader(self.lid)));
        } else {
            // TODO: This is probably not exactly safe.
            let query = request.get_query().unwrap();
            let result = self.state_machine.read().unwrap().query(query);
            let message = messages::command_response_success(&result, self.lid);
            actions.client_messages.push((from, message));
        }
    }

    fn into_reader<C>(message: &Builder<C>) -> Reader<OwnedSegments>
        where C: Allocator
    {
        let mut buf = Cursor::new(Vec::new());

        serialize::write_message(&mut buf, message).unwrap();
        buf.set_position(0);
        serialize::read_message(&mut buf, ReaderOptions::new()).unwrap()
    }

    /// Triggers a heartbeat timeout for the peer.
    fn heartbeat_timeout(&mut self, peer: ServerId, actions: &mut Actions) {
        scoped_assert!(self.is_leader());
        scoped_debug!("HeartbeatTimeout for peer: {}", peer);
        let mut message = Builder::new_default();
        {
            let mut request = message.init_root::<message::Builder>();
            request.set_log_id(&self.lid.as_bytes());
            let mut request = request.init_append_entries_request();
            request.set_term(self.current_term().as_u64());
            request.set_prev_log_index(self.latest_log_index().as_u64());
            request.set_prev_log_term(self.log.latest_log_term().unwrap().as_u64());
            request.set_leader_commit(self.commit_index.as_u64());
            request.init_entries(0);
        }
        let message = Rc::new(message);
        actions.peer_messages.push((peer, message.clone()));
    }

    /// Triggers an election timeout.
    fn election_timeout(&mut self, actions: &mut Actions) {
        scoped_assert!(!self.is_leader());
        if self.peers.is_empty() {
            // Solitary replica special case; jump straight to Leader state.
            scoped_info!("ElectionTimeout: transitioning to Leader");
            scoped_assert!(self.is_follower());
            scoped_assert!(self.log.voted_for().unwrap().is_none());
            self.log.inc_current_term().unwrap();
            self.log.set_voted_for(Some(self.id)).unwrap();
            let latest_log_index = self.latest_log_index();
            self.state = ConsensusState::Leader;
            self.leader_state.write().unwrap().reinitialize(latest_log_index);
        } else {
            scoped_info!("ElectionTimeout: transitioning to Candidate");
            self.transition_to_candidate(actions);
        }
    }

    /// Transitions this consensus state machine to Leader state.
    fn transition_to_leader(&mut self, actions: &mut Actions) {
        scoped_trace!("transitioning to Leader");
        let current_term = self.current_term();
        let latest_log_index = self.latest_log_index();
        let latest_log_term = self.log.latest_log_term().unwrap();
        self.state = ConsensusState::Leader;
        self.leader_state.write().unwrap().reinitialize(latest_log_index);

        let message = messages::append_entries_request(current_term,
                                                       latest_log_index,
                                                       latest_log_term,
                                                       &[],
                                                       self.commit_index,
                                                       &self.lid);
        for &peer in self.peers().keys() {
            actions.peer_messages.push((peer, message.clone()));
        }

        // reset transaction

        if self.transaction.is_active {
            self.transaction.broadcast_rollback(self.lid, actions);

            let (commit_index, last_applied, _) = self.transaction
                .rollback()
                .unwrap();
            self.commit_index = commit_index;
            self.last_applied = last_applied;

            {
                let entries_failed = self.log.rollback(commit_index).unwrap();

                for &(_, ref command) in entries_failed.iter().rev() {
                    self.state_machine.write().unwrap().revert(command.as_slice());
                }
            }
        }

        actions.clear_timeouts.push(self.lid);
        actions.clear_peer_messages = true;
    }

    /// Transitions the consensus state machine to Candidate state.
    fn transition_to_candidate(&mut self, actions: &mut Actions) {
        scoped_trace!("transitioning to Candidate");
        self.log.inc_current_term().unwrap();
        self.log.set_voted_for(Some(self.id)).unwrap();
        self.state = ConsensusState::Candidate;
        let mut candidate_state = self.candidate_state.write().unwrap();
        candidate_state.clear();
        candidate_state.record_vote(self.id);

        let message = messages::request_vote_request(self.current_term(),
                                                     self.latest_log_index(),
                                                     self.log.latest_log_term().unwrap(),
                                                     &self.lid);

        for &peer in self.peers().keys() {
            actions.peer_messages.push((peer, message.clone()));
        }
        actions.timeouts.push(ConsensusTimeout::Election(self.lid));
        actions.clear_peer_messages = true;
    }

    /// Advances the commit index and applies committed entries to the state machine.
    fn advance_commit_index(&mut self, actions: &mut Actions) {
        scoped_assert!(self.is_leader());
        let majority = self.majority();
        {
            let leader_state = self.leader_state.read().unwrap();
            // TODO: Figure out failure condition here.
            while self.commit_index < self.log.latest_log_index().unwrap() {
                if leader_state.count_match_indexes(self.commit_index + 1) >= majority {
                    self.commit_index = self.commit_index + 1;
                    scoped_debug!("commit index advanced to {}", self.commit_index);
                } else {
                    break; // If there isn't a majority now, there won't be one later.
                }
            }
        }

        let results = self.apply_commits();
        let mut leader_state = self.leader_state.write().unwrap();

        // TODO: Figure out failure condition here.
        while let Some(&(client, index)) = leader_state.proposals.get(0) {
            if index <= self.commit_index {
                scoped_trace!("responding to client {} for entry {}", client, index);
                // We know that there will be an index here since it was commited
                // and the index is less than that which has been commited.
                let ref result = results[&index];
                let message = messages::command_response_success(result.as_slice(), self.lid);
                actions.client_messages.push((client, message));
                leader_state.proposals.pop_front();
            } else {
                break;
            }
        }
    }

    /// Applies all committed but unapplied log entries to the state machine.  Returns the set of
    /// return values from the commits applied.
    fn apply_commits(&mut self) -> HashMap<LogIndex, Vec<u8>> {
        let mut results = HashMap::new();
        while self.last_applied < self.commit_index {
            // Unwrap justified here since we know there is an entry here.
            let (_, entry) = match self.log.entry(self.last_applied + 1) {
                Ok(e) => e,
                Err(_) => break,
            };

            if !entry.is_empty() {
                let result = self.state_machine.write().unwrap().apply(entry);
                results.insert(self.last_applied + 1, result);
            }
            self.last_applied = self.last_applied + 1;
        }
        results
    }

    /// Transitions the consensus state machine to Follower state with the provided term. The
    /// `voted_for` field will be reset. The provided leader hint will replace the last known
    /// leader.
    fn transition_to_follower(&mut self, term: Term, leader: ServerId, actions: &mut Actions) {
        scoped_trace!("transitioning to Follower");
        self.log.set_current_term(term).unwrap();
        self.state = ConsensusState::Follower;
        self.follower_state.write().unwrap().set_leader(leader);
        actions.clear_timeouts.push(self.lid);
        actions.clear_peer_messages = true;
        actions.timeouts.push(ConsensusTimeout::Election(self.lid));
    }

    /// Returns whether the consensus state machine is currently a Leader.
    pub fn is_leader(&self) -> bool {
        self.state == ConsensusState::Leader
    }

    /// Returns whether the consensus state machine is currently a Follower.
    fn is_follower(&self) -> bool {
        self.state == ConsensusState::Follower
    }

    /// Returns whether the consensus state machine is currently a Candidate.
    fn is_candidate(&self) -> bool {
        self.state == ConsensusState::Candidate
    }

    /// Returns the current term.
    fn current_term(&self) -> Term {
        self.log.current_term().unwrap()
    }

    /// Returns the term of the latest applied log entry.
    fn latest_log_term(&self) -> Term {
        self.log.latest_log_term().unwrap()
    }

    /// Returns the index of the latest applied log entry.
    fn latest_log_index(&self) -> LogIndex {
        self.log.latest_log_index().unwrap()
    }

    /// Get the cluster quorum majority size.
    fn majority(&self) -> usize {
        let peers = self.peers.len();
        let cluster_members = peers.checked_add(1)
            .expect(&format!("unable to support {} cluster members", peers));
        (cluster_members >> 1) + 1
    }
}

impl<L, M> fmt::Debug for Consensus<L, M>
    where L: Log,
          M: StateMachine
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self.state {
            ConsensusState::Follower => {
                write!(fmt,
                       "Follower {{ lid: {}, term: {}, index: {} }}",
                       self.lid,
                       self.current_term(),
                       self.latest_log_index())
            }
            ConsensusState::Candidate => {
                write!(fmt,
                       "Candidate {{ lid: {}, term: {}, index: {} }}",
                       self.lid,
                       self.current_term(),
                       self.latest_log_index())
            }
            ConsensusState::Leader => {
                write!(fmt,
                       "Leader {{ lid: {}, term: {}, index: {} }}",
                       self.lid,
                       self.current_term(),
                       self.latest_log_index())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate env_logger;
    extern crate test;

    use std::collections::{HashMap, VecDeque};
    use std::io::Cursor;
    use std::net::SocketAddr;
    use std::rc::Rc;
    use std::str::FromStr;

    use capnp::serialize::{self, OwnedSegments};
    use capnp::message::{Allocator, Builder, HeapAllocator, Reader, ReaderOptions};
    use messages_capnp::{client_request, message};
    use ClientId;
    use LogIndex;
    use ServerId;
    use LogId;
    use Term;
    use TransactionId;
    use messages;
    use consensus::{Actions, Consensus, ConsensusTimeout};
    use state_machine::NullStateMachine;
    use persistent_log::{MemLog, Log};
    use uuid::Uuid;
    use std::cell::RefCell;

    type TestPeer = Consensus<MemLog, NullStateMachine>;

    lazy_static!{
        static ref lid: LogId = LogId(Uuid::new_v4());
    }

    fn new_cluster(size: u64) -> HashMap<ServerId, TestPeer> {
        let ids: HashMap<ServerId, SocketAddr> = (0..size)
            .map(Into::into)
            .map(|id| (id, SocketAddr::from_str(&format!("127.0.0.1:{}", id)).unwrap()))
            .collect();
        ids.iter()
            .map(|(&id, _)| {
                let mut peers = ids.clone();
                peers.remove(&id);
                let store = MemLog::new();
                (id, Consensus::new(id, *lid, peers, store, NullStateMachine))
            })
            .collect()
    }

    fn into_reader<A>(message: &Builder<A>) -> Reader<OwnedSegments>
        where A: Allocator
    {
        let mut buf = Cursor::new(Vec::new());

        serialize::write_message(&mut buf, message).unwrap();
        buf.set_position(0);
        serialize::read_message(&mut buf, ReaderOptions::new()).unwrap()
    }

    /// Applies the actions to the consensus peers (and recursively applies any resulting
    /// actions), and returns any client messages.
    fn apply_actions(from: ServerId,
                     mut actions: Actions,
                     peers: &mut HashMap<ServerId, TestPeer>)
                     -> Vec<(ClientId, Rc<Builder<HeapAllocator>>)> {
        let mut queue: VecDeque<(ServerId, ServerId, Rc<Builder<HeapAllocator>>)> = VecDeque::new();

        for (to, message) in actions.peer_messages.iter().cloned() {
            queue.push_back((from, to, message));
        }
        actions.peer_messages.clear();

        while let Some((from, to, message)) = queue.pop_front() {
            let mut reader = into_reader(&*message);
            let message_reader = reader.get_root::<message::Reader>().unwrap();
            peers.get_mut(&to)
                .unwrap()
                .apply_peer_message(from, &message_reader, &mut actions);
            let inner_from = to;
            for (inner_to, message) in actions.peer_messages.iter().cloned() {
                queue.push_back((inner_from, inner_to, message));
            }
            actions.peer_messages.clear();
        }

        let Actions { client_messages, .. } = actions;
        client_messages
    }

    /// Elect `leader` as the leader of a cluster with the provided followers.
    /// The leader and the followers must be in the same term.
    fn elect_leader(leader: ServerId, peers: &mut HashMap<ServerId, TestPeer>) {
        let mut actions = Actions::new();
        peers.get_mut(&leader)
            .unwrap()
            .apply_timeout(ConsensusTimeout::Election(*lid), &mut actions);
        let client_messages = apply_actions(leader, actions, peers);
        assert!(client_messages.is_empty());
        assert!(peers[&leader].is_leader());
    }

    /// Tests the majority function.
    #[test]
    fn test_majority() {
        let (_, peer) = new_cluster(1).into_iter().next().unwrap();
        assert_eq!(1, peer.majority());

        let (_, peer) = new_cluster(2).into_iter().next().unwrap();
        assert_eq!(2, peer.majority());

        let (_, peer) = new_cluster(3).into_iter().next().unwrap();
        assert_eq!(2, peer.majority());

        let (_, peer) = new_cluster(4).into_iter().next().unwrap();
        assert_eq!(3, peer.majority());
    }

    /// Tests that a consensus state machine with no peers will transitition immediately to the
    /// Leader state upon the first election timeout.
    #[test]
    fn test_solitary_consensus_transition_to_leader() {
        setup_test!("test_solitary_consensus_transition_to_leader");
        let (_, mut peer) = new_cluster(1).into_iter().next().unwrap();
        assert!(peer.is_follower());

        let mut actions = Actions::new();
        peer.apply_timeout(ConsensusTimeout::Election(*lid), &mut actions);
        assert!(peer.is_leader());
        assert!(actions.peer_messages.is_empty());
        assert!(actions.client_messages.is_empty());
        assert!(actions.timeouts.is_empty());
    }

    /// A simple election test over multiple group sizes.
    #[test]
    fn test_election() {
        setup_test!("test_election");

        for group_size in 1..10 {
            let mut peers = new_cluster(group_size);
            let peer_ids: Vec<ServerId> = peers.keys().cloned().collect();
            let leader = &peer_ids[0];
            elect_leader(leader.clone(), &mut peers);
            assert!(peers[leader].is_leader());
            for follower in peer_ids.iter().skip(1) {
                assert!(peers[follower].is_follower());
            }
        }
    }

    /// Tests the Raft heartbeating mechanism. The leader receives a heartbeat
    /// timeout, and in response sends an AppendEntries message to the follower.
    /// The follower in turn resets its election timout, and replies to the
    /// leader.
    #[test]
    fn test_heartbeat() {
        setup_test!("test_heartbeat");
        let mut peers = new_cluster(2);
        let peer_ids: Vec<ServerId> = peers.keys().cloned().collect();
        let leader_id = &peer_ids[0];
        let follower_id = &peer_ids[1];
        elect_leader(leader_id.clone(), &mut peers);

        // Leader pings with a heartbeat timeout.
        let leader_append_entries = {
            let mut actions = Actions::new();
            let leader = peers.get_mut(&leader_id).unwrap();
            leader.heartbeat_timeout(follower_id.clone(), &mut actions);

            let peer_message = actions.peer_messages.iter().next().unwrap();
            assert_eq!(peer_message.0, follower_id.clone());
            peer_message.1.clone()
        };
        let mut reader = into_reader(&*leader_append_entries);
        let message_reader = reader.get_root::<message::Reader>().unwrap();

        // Follower responds.
        let follower_response = {
            let mut actions = Actions::new();
            let follower = peers.get_mut(&follower_id).unwrap();
            follower.apply_peer_message(leader_id.clone(), &message_reader, &mut actions);

            let election_timeout = actions.timeouts.iter().next().unwrap();
            assert_eq!(election_timeout, &ConsensusTimeout::Election(*lid));

            let peer_message = actions.peer_messages.iter().next().unwrap();
            assert_eq!(peer_message.0, leader_id.clone());
            peer_message.1.clone()
        };
        let mut reader = into_reader(&*follower_response);
        let message_reader = reader.get_root::<message::Reader>().unwrap();

        // Leader applies and sends back a heartbeat to establish leadership.
        let leader = peers.get_mut(&leader_id).unwrap();
        let mut actions = Actions::new();
        leader.apply_peer_message(follower_id.clone(), &message_reader, &mut actions);
        let heartbeat_timeout = actions.timeouts.iter().next().unwrap();
        assert_eq!(heartbeat_timeout,
                   &ConsensusTimeout::Heartbeat(follower_id.clone(), *lid));
    }

    /// Emulates a slow heartbeat message in a two-node cluster.
    ///
    /// The initial leader (Consensus 0) sends a heartbeat, but before it is received by the follower
    /// (Consensus 1), Consensus 1's election timeout fires. Consensus 1 transitions to candidate state
    /// and attempts to send a RequestVote to Consensus 0. When the partition is fixed, the
    /// RequestVote should prompt Consensus 0 to step down. Consensus 1 should send a stale term
    /// message in response to the heartbeat from Consensus 0.
    #[test]
    fn test_slow_heartbeat() {
        setup_test!("test_heartbeat");
        let mut peers = new_cluster(2);
        let peer_ids: Vec<ServerId> = peers.keys().cloned().collect();
        let peer_0 = &peer_ids[0];
        let peer_1 = &peer_ids[1];
        elect_leader(peer_0.clone(), &mut peers);

        let mut peer_0_actions = Actions::new();
        peers.get_mut(peer_0)
            .unwrap()
            .apply_timeout(ConsensusTimeout::Heartbeat(*peer_1, *lid),
                           &mut peer_0_actions);
        assert!(peers[peer_0].is_leader());

        let mut peer_1_actions = Actions::new();
        peers.get_mut(peer_1)
            .unwrap()
            .apply_timeout(ConsensusTimeout::Election(*lid), &mut peer_1_actions);
        assert!(peers[peer_1].is_candidate());

        // Apply candidate messages.
        assert!(apply_actions(*peer_1, peer_1_actions, &mut peers).is_empty());
        assert!(peers[peer_0].is_follower());
        assert!(peers[peer_1].is_leader());

        // Apply stale heartbeat.
        assert!(apply_actions(*peer_0, peer_0_actions, &mut peers).is_empty());
        assert!(peers[peer_0].is_follower());
        assert!(peers[peer_1].is_leader());
    }

    /// Tests that a client proposal is correctly replicated to peers, and the client is notified
    /// of the success.
    #[test]
    fn test_proposal() {
        setup_test!("test_proposal");
        // Test various sizes.
        for i in 1..7 {
            scoped_debug!("testing size {} cluster", i);
            let mut peers = new_cluster(i);
            let peer_ids: Vec<ServerId> = peers.keys().cloned().collect();
            let leader = peer_ids[0];
            elect_leader(leader, &mut peers);

            let value: &[u8] = b"foo";
            let reader =
                into_reader(&messages::proposal_request(TransactionId::new(), value, *lid));
            let message_reader = reader.get_root::<client_request::Reader>()
                .unwrap();
            let mut actions = Actions::new();

            let client = ClientId::new();

            peers.get_mut(&leader)
                .unwrap()
                .apply_client_message(client, &message_reader, &mut actions);

            let client_messages = apply_actions(leader, actions, &mut peers);
            assert_eq!(1, client_messages.len());
            for peer in peers.values() {
                assert_eq!((Term(1), value), peer.log.entry(LogIndex(1)).unwrap());
            }
        }
    }

    #[test]
    // Verify that out-of-order appends don't lead to the log tail being
    // dropped. See https://github.com/ktoso/akka-raft/issues/66; it's
    // not actually something that can happen in practice with TCP, but
    // wise to avoid it altogether.
    fn test_append_reorder() {
        setup_test!("test_append_reorder");
        let mut peers = new_cluster(2);
        let peer_ids: Vec<ServerId> = peers.keys().cloned().collect();
        let mut actions = Actions::new();
        let mut follower = peers.get_mut(&peer_ids[0]).unwrap();
        let value: &[u8] = b"foo";
        let entries = vec![(Term(1), value), (Term(1), value)];
        let reader = into_reader(&*messages::append_entries_request(Term(1),
                                                                    LogIndex(0),
                                                                    Term(0),
                                                                    &entries,
                                                                    LogIndex(0),
                                                                    &*lid));

        let msg1 = reader.get_root::<message::Reader>()
            .unwrap();
        let reader = into_reader(&*messages::append_entries_request(Term(1),
                                                                    LogIndex(0),
                                                                    Term(0),
                                                                    &entries[0..1],
                                                                    LogIndex(0),
                                                                    &*lid));
        let msg2 = reader.get_root::<message::Reader>()
            .unwrap();
        follower.apply_peer_message(peer_ids[1], &msg1, &mut actions);
        follower.apply_peer_message(peer_ids[1], &msg2, &mut actions);

        assert_eq!((Term(1), value), follower.log.entry(LogIndex(1)).unwrap());
        assert_eq!((Term(1), value), follower.log.entry(LogIndex(2)).unwrap());
    }

    #[bench]
    fn bench_proposal_1(b: &mut test::Bencher) {
        bench_n(b, 1)
    }

    #[bench]
    fn bench_proposal_3(b: &mut test::Bencher) {
        bench_n(b, 3)
    }

    #[bench]
    fn bench_proposal_5(b: &mut test::Bencher) {
        bench_n(b, 5)
    }

    fn bench_n(b: &mut test::Bencher, size: u64) {
        let mut peers = new_cluster(size);
        let peer_ids: Vec<ServerId> = peers.keys().cloned().collect();
        let leader = peer_ids[0];
        elect_leader(leader, &mut peers);

        let value: &[u8] = b"foo";
        let reader = into_reader(&messages::proposal_request(TransactionId::new(), value, *lid));
        let message_reader = reader.get_root::<client_request::Reader>()
            .unwrap();
        let client = ClientId::new();


        b.iter(|| {
            let mut actions = Actions::new();
            peers.get_mut(&leader)
                .unwrap()
                .apply_client_message(client, &message_reader, &mut actions);

            let client_messages = apply_actions(leader, actions, &mut peers);
            assert_eq!(1, client_messages.len());
        });
    }
}
