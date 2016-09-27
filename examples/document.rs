#![feature(plugin)]
#![feature(custom_derive)]
#![feature(drop_types_in_const)]

extern crate raft;

#[macro_use]
extern crate iron;
extern crate router;

#[macro_use]
extern crate log;
extern crate env_logger;

extern crate scoped_log;

extern crate docopt;
extern crate rustc_serialize;
extern crate bincode;

extern crate uuid;

use std::net::{SocketAddr, ToSocketAddrs, SocketAddrV4, Ipv4Addr};
use bincode::rustc_serialize::{encode, decode};
use bincode::SizeLimit;
use docopt::Docopt;

use raft::{Client, state_machine, persistent_log, ServerId};
use std::collections::HashMap;

use std::fs::File;
use std::fs::OpenOptions;
use std::fs::remove_file;
use std::io::Read;
use std::io::Write;
use std::error::Error;
use std::io::ErrorKind;

use Message::*;

use uuid::Uuid;

use iron::prelude::*;
use iron::status;
use router::Router;

static USAGE: &'static str = "
A replicated document database.

Commands:

    get     Returns document

    put     Set document

    server  Starts server

Usage:
    document get <doc-id> <node-address>
    document put <node-address> <filepath> <filename>
    document remove <doc-id> <node-address>
    document server <rest-port> <id> [<node-id> <node-address>]...
";

static mut state_machine: Option<DocumentStateMachine> = None;

#[derive(Debug,RustcDecodable,Clone)]
struct Args {
    cmd_server: bool,
    cmd_get: bool,
    cmd_put: bool,
    cmd_remove: bool,
    arg_id: Option<u64>,
    arg_doc_id: Option<String>,
    arg_filename: Option<String>,
    arg_node_id: Vec<u64>,
    arg_node_address: Vec<String>,
    arg_filepath: String,
    arg_rest_port: Option<u64>,
}

#[derive(RustcEncodable,RustcDecodable)]
pub enum Message {
    Get(Uuid),
    Put(Document),
    Remove(Uuid),
}

#[derive(RustcEncodable,RustcDecodable,Debug)]
pub struct Document {
    filename: String,
    payload: Vec<u8>,
}

fn main() {
    env_logger::init().unwrap();

    let args: Args = Docopt::new(USAGE)
        .and_then(|d| d.decode())
        .unwrap_or_else(|e| e.exit());

    if args.cmd_server {

        let mut router = Router::new();
        router.get("/document/:fileId", http_get, "get_document");
        router.post("/document/", http_post, "post_document");
        router.delete("/document/:fileId", http_delete, "delete_document");

        let rest_port = args.arg_rest_port.unwrap() as u16;

        std::thread::spawn(move || {
            Iron::new(router).http(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), rest_port));
        });

        // TODO refactor
        fn http_get(req: &mut Request) -> IronResult<Response> {
            unsafe {
                let sm = state_machine.clone().unwrap();

                let ref fileId = req.extensions
                    .get::<Router>()
                    .unwrap()
                    .find("fileId")
                    .unwrap();

                let document = sm.get(Uuid::parse_str(*fileId).unwrap());

                Ok(Response::with((status::Ok, format!("{:?}", document))))
            }
        }

        fn http_post(req: &mut Request) -> IronResult<Response> {
            Ok(Response::with((status::Ok, "Ok")))
        }

        fn http_delete(req: &mut Request) -> IronResult<Response> {
            Ok(Response::with((status::Ok, "Ok")))
        }

        server(&args);

    } else if args.cmd_get {
        get(&args);
    } else if args.cmd_put {
        put(&args);
    } else if args.cmd_remove {
        remove(&args);
    }
}

fn server(args: &Args) {

    let persistent_log = persistent_log::doc::DocLog::new();

    let id = ServerId::from(args.arg_id.unwrap());

    let mut peers = args.arg_node_id
        .iter()
        .zip(args.arg_node_address.iter())
        .map(|(&id, addr)| (ServerId::from(id), parse_addr(&addr)))
        .collect::<HashMap<_, _>>();

    let addr = peers.remove(&id).unwrap();

    unsafe {
        state_machine = Some(DocumentStateMachine::new());
        raft::Server::run(id,
                          addr,
                          peers,
                          persistent_log,
                          state_machine.clone().unwrap())
            .unwrap();
    }
}

fn get(args: &Args) {
    let cluster = args.arg_node_address.iter().map(|v| parse_addr(&v)).collect();

    let mut client = Client::new(cluster);

    let id: Uuid = match Uuid::parse_str(&args.arg_doc_id.clone().unwrap()) {
        Ok(id) => id,
        Err(err) => panic!("{} is not a valid id", args.arg_doc_id.clone().unwrap()),
    };

    let payload = encode(&Message::Get(id), SizeLimit::Infinite).unwrap();

    let response = client.query(payload.as_slice()).unwrap();

    let document: Document = decode(response.as_slice()).unwrap();

    let mut handler = OpenOptions::new()
        .read(false)
        .write(true)
        .create(true)
        .open(&document.filename)
        .unwrap();

    handler.write_all(document.payload.as_slice());

    println!("Written");
}

fn put(args: &Args) {
    let cluster = args.arg_node_address.iter().map(|v| parse_addr(&v)).collect();

    let mut client = Client::new(cluster);

    let filename = (&args.arg_filename).clone().unwrap();
    let mut handler = File::open(&args.arg_filepath).unwrap();
    let mut buffer: Vec<u8> = Vec::new();

    handler.read_to_end(&mut buffer);

    let document = Document {
        filename: filename,
        payload: buffer,
    };

    let payload = encode(&Message::Put(document), SizeLimit::Infinite).unwrap();

    let response = client.propose(payload.as_slice()).unwrap();

    println!("{}", String::from_utf8(response).unwrap())
}

fn remove(args: &Args) {
    let cluster = args.arg_node_address.iter().map(|v| parse_addr(&v)).collect();

    let mut client = Client::new(cluster);

    let id: Uuid = match Uuid::parse_str(&args.arg_doc_id.clone().unwrap()) {
        Ok(id) => id,
        Err(err) => panic!("{} is not a valid id", &args.arg_doc_id.clone().unwrap()),
    };

    let payload = encode(&Message::Remove(id), SizeLimit::Infinite).unwrap();

    let response = client.propose(payload.as_slice()).unwrap();

    println!("{}", String::from_utf8(response).unwrap())
}

fn parse_addr(addr: &str) -> SocketAddr {
    addr.to_socket_addrs()
        .ok()
        .expect(&format!("unable to parse socket address: {}", addr))
        .next()
        .unwrap()
}

#[derive(Debug,Clone)]
pub struct DocumentStateMachine {
    map: HashMap<Uuid, String>,
}

impl DocumentStateMachine {
    pub fn new() -> Self {
        DocumentStateMachine { map: HashMap::new() }
    }

    pub fn get(&self, id: Uuid) -> Result<Document, std::io::Error> {
        let mut handler = try!(File::open(format!("data/{}", id)));

        let mut content: Vec<u8> = Vec::new();
        try!(handler.read_to_end(&mut content));

        let name = self.map.get(&id);

        match name {
            Some(name) => {
                Ok(Document {
                    filename: name.clone(),
                    payload: content,
                })
            } 
            None => {
                Err(std::io::Error::new(ErrorKind::Other,
                                        format!("No entry with the id \"{}\" found", id)))
            }
        }
    }

    pub fn put(&mut self, document: Document) -> Result<Uuid, std::io::Error> {
        let id = Uuid::new_v4();
        self.map.insert(id, document.filename.clone());

        let mut handler = try!(OpenOptions::new()
            .read(false)
            .write(true)
            .create(true)
            .open(format!("data/{}", id)));

        try!(handler.write_all(document.payload.as_slice()));

        Ok(id)
    }

    pub fn remove(&mut self, id: Uuid) -> Result<&str, std::io::Error> {
        self.map.remove(&id);
        try!(remove_file(format!("data/{}", id)));
        Ok("Document deleted")
    }
}

impl state_machine::StateMachine for DocumentStateMachine {
    fn apply(&mut self, new_value: &[u8]) -> Vec<u8> {
        let message = decode(&new_value).unwrap();

        let response = match message {
            Get(id) => self.query(new_value),
            Put(document) => {
                match self.put(document) {
                    Ok(id) => encode(&id, SizeLimit::Infinite).unwrap(),
                    Err(err) => encode(&err.description(), SizeLimit::Infinite).unwrap(),
                }
            }
            Remove(id) => {
                match self.remove(id) {
                    Ok(id) => encode(&id, SizeLimit::Infinite).unwrap(),
                    Err(err) => encode(&err.description(), SizeLimit::Infinite).unwrap(),
                }
            }
        };

        response
    }

    fn query(&self, query: &[u8]) -> Vec<u8> {
        let message = decode(&query).unwrap();

        let response = match message {
            Get(id) => {
                match self.get(id) {
                    Ok(document) => encode(&document, SizeLimit::Infinite).unwrap(),
                    Err(err) => encode(&err.description(), SizeLimit::Infinite).unwrap(),
                }
            }
            _ => {
                let response = encode(&"Wrong usage of .query()", SizeLimit::Infinite);

                response.unwrap()
            }
        };

        response
    }

    fn snapshot(&self) -> Vec<u8> {
        encode(&self.map, SizeLimit::Infinite).unwrap()
    }

    fn restore_snapshot(&mut self, snapshot_value: Vec<u8>) {
        self.map = decode(&snapshot_value).unwrap();
        ()
    }
}
