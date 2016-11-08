use std::{error, fmt, result};
use std::fs::File;
use std::io::prelude::*;
use bincode::SizeLimit;
use bincode::rustc_serialize::{encode_into, encode, decode, decode_from};
use std::fs::OpenOptions;

use raft::persistent_log::Log;
use raft::LogIndex;
use raft::ServerId;
use raft::Term;

#[derive(Clone, Debug)]
pub struct DocLog {
    entries: Vec<(Term, Vec<u8>)>,
}

/// Non-instantiable error type for MemLog
pub enum Error { }

impl fmt::Display for Error {
    fn fmt(&self, _fmt: &mut fmt::Formatter) -> fmt::Result {
        unreachable!()
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, _fmt: &mut fmt::Formatter) -> fmt::Result {
        unreachable!()
    }
}

impl error::Error for Error {
    fn description(&self) -> &str {
        unreachable!()
    }
}

impl DocLog {
    pub fn new() -> Self {
        let mut d = DocLog { entries: Vec::new() };

        d.set_current_term(Term::from(0));

        d
    }
}

// TODO error handling for IO
impl Log for DocLog {
    type Error = Error;

    fn current_term(&self) -> result::Result<Term, Error> {
        let mut term_handler = File::open("term").expect("Could not find term file");

        let term: Term = decode_from(&mut term_handler, SizeLimit::Infinite).unwrap();

        Ok(term)
    }

    fn set_current_term(&mut self, term: Term) -> result::Result<(), Error> {
        let mut term_handler = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open("term")
            .unwrap();

        encode_into(&term, &mut term_handler, SizeLimit::Infinite);

        self.set_voted_for(None);

        Ok(())
    }

    fn inc_current_term(&mut self) -> result::Result<Term, Error> {
        self.set_voted_for(None);
        let new_term = self.current_term().unwrap() + 1;
        self.set_current_term(new_term);
        self.current_term()
    }

    fn voted_for(&self) -> result::Result<Option<ServerId>, Error> {
        let mut voted_for_handler = File::open("voted_for").expect("Could not find voted_for file");

        let voted_for: Option<ServerId> = decode_from(&mut voted_for_handler, SizeLimit::Infinite)
            .unwrap();

        Ok(voted_for)
    }

    fn set_voted_for(&mut self, address: Option<ServerId>) -> result::Result<(), Error> {
        let mut voted_for_handler = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open("voted_for")
            .unwrap();

        encode_into(&address, &mut voted_for_handler, SizeLimit::Infinite);

        Ok(())
    }

    fn latest_log_index(&self) -> result::Result<LogIndex, Error> {
        Ok(LogIndex::from(self.entries.len() as u64))
    }

    fn latest_log_term(&self) -> result::Result<Term, Error> {
        let len = self.entries.len();
        if len == 0 {
            Ok(Term::from(0))
        } else {
            Ok(self.entries[len - 1].0)
        }
    }

    fn entry(&self, index: LogIndex) -> result::Result<(Term, &[u8]), Error> {
        let (term, ref bytes) = self.entries[(index - 1).as_u64() as usize];
        Ok((term, bytes))
    }

    fn append_entries(&mut self,
                      from: LogIndex,
                      entries: &[(Term, &[u8])])
                      -> result::Result<(), Error> {
        assert!(self.latest_log_index().unwrap() + 1 >= from);
        self.entries.truncate((from - 1).as_u64() as usize);
        Ok(self.entries.extend(entries.iter().map(|&(term, command)| (term, command.to_vec()))))
    }

    fn truncate(&mut self, lo: LogIndex) -> result::Result<(), Error> {
        Ok(self.entries.truncate(lo.as_u64() as usize))
    }

    fn rollback(&mut self, lo: LogIndex) -> result::Result<(Vec<(Term, Vec<u8>)>), Error> {
        Ok(self.entries[(lo.as_u64() as usize)..].to_vec())
    }
}

#[cfg(test)]
mod test {

    use super::*;
    use raft::LogIndex;
    use raft::ServerId;
    use raft::Term;
    use raft::persistent_log::Log;
    use std::fs::File;
    use bincode::SizeLimit;
    use bincode::rustc_serialize::{encode_into, encode, decode, decode_from};
    use std::io::prelude::*;
    use std::fs::OpenOptions;
    use std::io::SeekFrom;

    #[test]
    fn test_current_term() {
        let mut store = DocLog::new();
        assert_eq!(Term::from(0), store.current_term().unwrap());
        store.set_voted_for(Some(ServerId::from(0))).unwrap();
        store.set_current_term(Term::from(42)).unwrap();
        assert_eq!(None, store.voted_for().unwrap());
        assert_eq!(Term::from(42), store.current_term().unwrap());
        store.inc_current_term().unwrap();
        assert_eq!(Term::from(43), store.current_term().unwrap());
    }

    #[test]
    fn test_voted_for() {
        let mut store = DocLog::new();
        assert_eq!(None, store.voted_for().unwrap());
        let id = ServerId::from(0);
        store.set_voted_for(Some(id)).unwrap();
        assert_eq!(Some(id), store.voted_for().unwrap());
    }

    #[test]
    fn test_append_entries() {
        let mut store = DocLog::new();
        assert_eq!(LogIndex::from(0), store.latest_log_index().unwrap());
        assert_eq!(Term::from(0), store.latest_log_term().unwrap());

        // [0.1, 0.2, 0.3, 1.4]
        store.append_entries(LogIndex::from(1),
                            &[(Term::from(0), &[1]),
                              (Term::from(0), &[2]),
                              (Term::from(0), &[3]),
                              (Term::from(1), &[4])])
            .unwrap();
        assert_eq!(LogIndex::from(4), store.latest_log_index().unwrap());
        assert_eq!(Term::from(1), store.latest_log_term().unwrap());
        assert_eq!((Term::from(0), &*vec![1u8]),
                   store.entry(LogIndex::from(1)).unwrap());
        assert_eq!((Term::from(0), &*vec![2u8]),
                   store.entry(LogIndex::from(2)).unwrap());
        assert_eq!((Term::from(0), &*vec![3u8]),
                   store.entry(LogIndex::from(3)).unwrap());
        assert_eq!((Term::from(1), &*vec![4u8]),
                   store.entry(LogIndex::from(4)).unwrap());

        // [0.1, 0.2, 0.3]
        store.append_entries(LogIndex::from(4), &[]).unwrap();
        assert_eq!(LogIndex::from(3), store.latest_log_index().unwrap());
        assert_eq!(Term::from(0), store.latest_log_term().unwrap());
        assert_eq!((Term::from(0), &*vec![1u8]),
                   store.entry(LogIndex::from(1)).unwrap());
        assert_eq!((Term::from(0), &*vec![2u8]),
                   store.entry(LogIndex::from(2)).unwrap());
        assert_eq!((Term::from(0), &*vec![3u8]),
                   store.entry(LogIndex::from(3)).unwrap());

        // [0.1, 0.2, 2.3, 3.4]
        store.append_entries(LogIndex::from(3),
                            &[(Term::from(2), &[3]), (Term::from(3), &[4])])
            .unwrap();
        assert_eq!(LogIndex::from(4), store.latest_log_index().unwrap());
        assert_eq!(Term::from(3), store.latest_log_term().unwrap());
        assert_eq!((Term::from(0), &*vec![1u8]),
                   store.entry(LogIndex::from(1)).unwrap());
        assert_eq!((Term::from(0), &*vec![2u8]),
                   store.entry(LogIndex::from(2)).unwrap());
        assert_eq!((Term::from(2), &*vec![3u8]),
                   store.entry(LogIndex::from(3)).unwrap());
        assert_eq!((Term::from(3), &*vec![4u8]),
                   store.entry(LogIndex::from(4)).unwrap());
    }
}