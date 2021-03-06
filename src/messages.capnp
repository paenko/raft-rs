@0x9293205eb3070c0f;

struct ConnectionPreamble {
    # Every connection opened to a Raft server, whether it is from a peer server
    # or a client, must begin with a ConnectionPreamble message. The Raft server
    # will not reply to this message, and it is safe for the connecting process
    # to immediately begin sending further messages. The connecting process must
    # include its ID, which indicates if the connecting process is a server or
    # client.

    id :union {
        server @0 :Peer;
        # Indicates that the connecting process is a Raft peer, and that all
        # further messages in the connection (in both directions) will be of
        # type Message.

        client @1 :Client;
        # Indicates that the connecting process is a client, and that all
        # further messages sent by the client will be of type ClientRequest, and
        # all replys from the server to the client will be of type
        # ClientResponse.

	serverAdd @2 :ServerAdd;
    }
}

struct ServerAdd{
   id @0 :UInt64;
   addr @1 :Text;
   community @2 :Text;
}

struct Peer {
   id @0 :UInt64;

   addr @1 :Text;

   community @2 :Text;

   peers @3 :List(PeerEntry);
}

struct PeerEntry{
   id @0 :UInt64;
   addr @1 :Text;
}

struct Client{
  id @3 :Data;
  username @2 :Text;
  password @0 :Text;

  data @1 :Data;
}

struct Entry {
    # A log entry.

    term @0 :UInt64;
    # The term of the entry.

    data @1 :Data;
    # The user-defined data of the entry.
}

struct Message {
    logId @7 :Data;

    union {
        appendEntriesRequest @0 :AppendEntriesRequest;
        appendEntriesResponse @1 :AppendEntriesResponse;
        requestVoteResponse @2 :RequestVoteResponse;
        requestVoteRequest @3 :RequestVoteRequest;
        transactionBegin @4 :TransactionBegin;
        transactionCommit @5 :TransactionCommit;
        transactionRollback @6 :TransactionRollback;
    }
}

struct TransactionBegin{
  session @0 :Data;
}

struct TransactionCommit{
  session @0 :Data;
}

struct TransactionRollback{
  session @0 :Data;
}

struct AppendEntriesRequest {

  term @0 :UInt64;
  # The leader's term.

  prevLogIndex @1 :UInt64;
  # Index of log entry immediately preceding new ones.

  prevLogTerm @2 :UInt64;
  # Term of prevLogIndex entry.

  entries @3 :List(Entry);
  # Log entries to store (empty for heartbeat; may send more than one for
  # efficiency).

  leaderCommit @4 :UInt64;
  # The Leader’s commit log index.
}

struct AppendEntriesResponse {

  term @0 :UInt64;
  # The responder's current term.

  union {
    success @1 :UInt64;
    # The `AppendEntries` request was a success. The responder's latest log
    # index is returned.

    staleTerm @2 :Void;
    # The `AppendEntries` request failed because the follower has a greater term
    # than the leader.

    inconsistentPrevEntry @3 :UInt64;
    # The `AppendEntries` request failed because the follower failed the
    # previous entry term and index checks. Includes the index of the
    # inconsistent entry.

    internalError @4 :Text;
    # an internal error occured; a description is included.
  }
}

struct RequestVoteRequest {

  term @0 :UInt64;
  # The candidate's term.

  lastLogIndex @1 :UInt64;
  # The index of the candidate's last log entry.

  lastLogTerm @2 :UInt64;
  # The term of the candidate's last log entry.
}

struct RequestVoteResponse {

  term @0 :UInt64;
  # The responder's current term.

  union {
    granted @1 :Void;
    # The voter voted for the candidate.

    staleTerm @2 :Void;
    # The `RequestVote` request failed because the voter has a greater term
    # than the candidate.

    alreadyVoted @3 :Void;
    # The voter did not vote for the candidate, because the voter already voted
    # in the term.

    inconsistentLog @4 :Void;
    # The `RequestVote` request failed because the candidate's log is not
    # up-to-date with the voter's log.

    internalError @5 :Text;
    # An internal error occurred; a description is included.
  }
}

struct AddPeerRequest{
  nodeId @0 :UInt64;
  nodeAddress @1 :Text;
}

struct AddPeerResponse{
  nodeId @0 :UInt64;
  # Information about which node has been added

  union{
	success @1 :Void;
	failure @2 :Text;
  }
}

struct ClientRequest {
  logId @6 :Data;
  
  union {
    ping @0 :PingRequest;
    proposal @1 :ProposalRequest;
    query @2 :QueryRequest;
    transactionBegin @3 :CliTransactionBegin;
    transactionCommit @4 :CliTransactionCommit;
    transactionRollback @5 :CliTransactionRollback;
  }
}

struct CliTransactionBegin{
  from @0 :Data;
  session @1 :Data;
  logIndex @2 :UInt64;
}

struct CliTransactionCommit{
  session @0 :Data;
}

struct CliTransactionRollback{
  session @0 :Data;
}

struct ClientResponse {
  logId @4 :Data;

  union {
    ping @0 :PingResponse;
    proposal @1 :CommandResponse;
    query @2 :CommandResponse;
    transaction @3 :CommandResponse;
  }
}

struct PingRequest {
  session @0 :Data;
}

struct PingResponse {

  term @0 :UInt64;
  # The server's current term.

  index @1 :UInt64;
  # The server's current index.

  state :union {
  # The server's current state.
    leader @2 :Void;
    follower @3 :Void;
    candidate @4 :Void;
  }
}

struct ProposalRequest {
  entry @0 :Data;
  # An entry to append.
  session @1 :Data;
}

struct QueryRequest {
    query @0 :Data;
    # An query to issue to the state machine.
}

struct CommandResponse {
  union {
    success @0 :Data;
    # The request succeeded.

    unknownLeader @1 :Void;
    # The proposal failed because the Raft node is not the leader, and does
    # not know who the leader is.

    notLeader @2 :Text;
    # The client request failed because the Raft node is not the leader.
    # The value returned may be the address of the current leader.

    failure @3 :Data;
  }
}
