use iron::status;
use router::Router;
use iron::prelude::*;
use params::{Params, Value};
use bodyparser;

use std::fs::read_dir;

use uuid::Uuid;
use std::net::{SocketAddr, ToSocketAddrs, SocketAddrV4, Ipv4Addr};

use std::error::Error;

use document::*;
use handler::Handler;
use statemachine::DocumentStateMachine;
use doclog::DocLog;

use std::thread::spawn;
use std::collections::HashSet;
use std::boxed::Box;

use raft::LogId;
use raft::state::{LeaderState, CandidateState, FollowerState};

use std::collections::HashMap;
use std::sync::{Arc, RwLock, Mutex};

use rustc_serialize::base64::{self, ToBase64, FromBase64, STANDARD};
use serde_json;
use serde_json::to_string as to_json;

#[derive(Deserialize,Serialize)]
struct http_Response {
    payload: String,
    version: usize,
}

#[derive(Clone,Copy)]
struct Context {
    node_addr: SocketAddrV4,
}

pub fn init(binding_addr: SocketAddr,
            node_addr: SocketAddrV4,
            states: HashMap<LogId,
                            (Arc<RwLock<LeaderState>>,
                             Arc<RwLock<CandidateState>>,
                             Arc<RwLock<FollowerState>>)>,
            state_machines: HashMap<LogId, Arc<DocumentStateMachine>>) {
    let mut router = Router::new();

    let states = Arc::new(states);
    let state_machines = Arc::new(state_machines);
    let context = Context { node_addr: node_addr };

    router.get("/document/:lid/:fileId",
               move |request: &mut Request| http_get(request, &context),
               "get_document");
    router.post("/document/:lid",
                move |request: &mut Request| http_post(request, &context),
                "post_document");
    router.post("/document/:lid/transaction/:session",
                move |request: &mut Request| http_trans_post(request, &context),
                "post_trans_document");
    router.delete("/document/:lid/:fileId",
                  move |request: &mut Request| http_delete(request, &context),
                  "delete_document");
    router.delete("/document/:lid/:fileId/transaction/:session",
                  move |request: &mut Request| http_trans_delete(request, &context),
                  "delete_trans_document");
    router.put("/document/:lid",
               move |request: &mut Request| http_put(request, &context),
               "put_document");
    router.put("/document/:lid/transaction/:session",
               move |request: &mut Request| http_put(request, &context),
               "put_trans_document");
    router.post("/transaction/begin/:lid",
                move |request: &mut Request| http_begin_transaction(request, &context),
                "begin_transaction");

    router.post("/transaction/commit/:lid",
                move |request: &mut Request| http_commit_transaction(request, &context),
                "commit_transaction");

    router.post("/transaction/rollback/:lid",
                move |request: &mut Request| http_rollback_transaction(request, &context),
                "rollback_transaction");

    {
        let state_machines = state_machines.clone();
        router.get("/meta/log/:lid/documents",
                   move |request: &mut Request| {
                       http_get_documents(request, &context, state_machines.clone())
                   },
                   "get_document_keys");

    }
    {
        let states = states.clone();
        router.get("/meta/logs",
                   move |request: &mut Request| http_logs(request, &context, states.clone()),
                   "meta_logs");
    }
    {
        let states = states.clone();
        router.get("/meta/:lid/state/leader",
                   move |request: &mut Request| {
                       http_meta_state_leader(request, &context, states.clone())
                   },
                   "meta_state_leader");
    }
    {
        let states = states.clone();
        router.get("/meta/:lid/state/candidate",
                   move |request: &mut Request| {
                       http_meta_state_candidate(request, &context, states.clone())
                   },
                   "meta_state_candidate");
    }
    {
        router.get("/meta/:lid/state/follower",
                   move |request: &mut Request| {
                       http_meta_state_follower(request, &context, states.clone())
                   },
                   "meta_state_follower");
    }

    fn http_get_documents(req: &mut Request,
                          context: &Context,
                          state_machines: Arc<HashMap<LogId, Arc<DocumentStateMachine>>>)
                          -> IronResult<Response> {
        let raw_lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"),
                               (status::BadRequest, "No lid found"));
        let lid = itry!(LogId::from(raw_lid),
                        (status::BadRequest, "LogId is invalid"));

        let state_machine = iexpect!(state_machines.get(&lid),
                                     (status::BadRequest, "No log found"));

        let documents = state_machine.get_documents();

        Ok(Response::with((status::Ok,
                           format!("{:?}",
                                   documents.iter()
                                       .map(|d| d.simple().to_string())
                                       .collect::<Vec<_>>()))))
    }

    fn http_logs(req: &mut Request,
                 context: &Context,
                 state: Arc<HashMap<LogId,
                                    (Arc<RwLock<LeaderState>>,
                                     Arc<RwLock<CandidateState>>,
                                     Arc<RwLock<FollowerState>>)>>)
                 -> IronResult<Response> {
        let keys = state.keys();

        let mut logs = String::new();

        for k in keys {
            logs.push('\n');
            logs.push_str(&format!("{}", k));
        }

        Ok(Response::with((status::Ok, format!("{}", &logs))))
    }

    fn http_meta_state_leader(req: &mut Request,
                              context: &Context,
                              state: Arc<HashMap<LogId,
                                                 (Arc<RwLock<LeaderState>>,
                                                  Arc<RwLock<CandidateState>>,
                                                  Arc<RwLock<FollowerState>>)>>)
                              -> IronResult<Response> {

        let raw_lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"),
                               (status::BadRequest, "No lid found"));
        let lid = itry!(LogId::from(raw_lid),
                        (status::BadRequest, "LogId is invalid"));

        let lock = state.get(&lid).unwrap().0.read().expect("Could not lock state");

        let ref lock = *lock;

        let json = to_json(&lock.clone()).expect("Cannot encode json");

        Ok(Response::with((status::Ok, format!("{}", json))))
    }

    fn http_meta_state_candidate(req: &mut Request,
                                 context: &Context,
                                 state: Arc<HashMap<LogId,
                                                    (Arc<RwLock<LeaderState>>,
                                                     Arc<RwLock<CandidateState>>,
                                                     Arc<RwLock<FollowerState>>)>>)
                                 -> IronResult<Response> {

        let raw_lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"),
                               (status::BadRequest, "Cannot find logid"));
        let lid = itry!(LogId::from(raw_lid), (status::BadRequest, "Invalid logid"));
        let lock = state.get(&lid).unwrap().1.read().expect("Could not lock state");

        Ok(Response::with((status::Ok, format!("{}", to_json(&*lock).unwrap()))))
    }

    fn http_meta_state_follower(req: &mut Request,
                                context: &Context,
                                state: Arc<HashMap<LogId,
                                                   (Arc<RwLock<LeaderState>>,
                                                    Arc<RwLock<CandidateState>>,
                                                    Arc<RwLock<FollowerState>>)>>)
                                -> IronResult<Response> {
        let raw_lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"),
                               (status::BadRequest, "Cannot find logid"));
        let lid = itry!(LogId::from(raw_lid), (status::BadRequest, "Invalid logid"));
        let lock = state.get(&lid).unwrap().2.read().expect("Could not lock state");

        Ok(Response::with((status::Ok, format!("{}", to_json(&*lock).unwrap()))))
    }

    spawn(move || {
        Iron::new(router).http(binding_addr);
    });

    fn http_get(req: &mut Request, context: &Context) -> IronResult<Response> {
        let ref fileId = req.extensions
            .get::<Router>()
            .unwrap()
            .find("fileId")
            .unwrap();
        let ref lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"),
                               (status::BadRequest, "Cannot find logid"));
        let ref username = iexpect!(req.extensions.get::<Router>().unwrap().find("username"),
                               (status::BadRequest, "Cannot find username"));
        let ref password = iexpect!(req.extensions.get::<Router>().unwrap().find("password"),
                               (status::BadRequest, "Cannot find password"));

        let document = Handler::get(&SocketAddr::V4(context.node_addr),
                                    &username,
                                    &password,
                                    &Uuid::parse_str(*fileId).unwrap(),
                                    &LogId::from(lid).unwrap())
            .unwrap();

        let http_doc = http_Response {
            version: document.version,
            payload: document.payload.as_slice().to_base64(STANDARD),
        };

        let encoded = itry!(to_json(&http_doc), "Cannot encode document to json");

        Ok(Response::with((status::Ok, encoded)))
    }

    fn http_post(req: &mut Request, context: &Context) -> IronResult<Response> {
        let payload = {
            let ref body = req.get::<bodyparser::Json>().unwrap().unwrap();

            let p = iexpect!(body.find("payload"),
                             (status::BadRequest, "No payload was in the body defined"));

            let str_payload = match *p {
                serde_json::Value::String(ref load) => load,
                _ => panic!("Unexpected payload type"),
            };

            str_payload.from_base64().expect("Payload is not base64")
        };


        let ref lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"));

        let username = "username";
        let password = "password";

        let id = Uuid::new_v4();

        let document = Document {
            id: id,
            payload: payload,
            version: 1,
        };

        let session = Uuid::new_v4();

        match Handler::post(&SocketAddr::V4(context.node_addr),
                            &username,
                            &password,
                            document,
                            &session,
                            &LogId::from(lid).unwrap()) {
            Ok(id) => Ok(Response::with((status::Ok, format!("{}", id)))),
            Err(err) => {
                Ok(Response::with((status::InternalServerError,
                                   "An error occured when posting new document")))
            }
        }
    }

    fn http_trans_post(req: &mut Request, context: &Context) -> IronResult<Response> {
        let payload = {
            let ref body = req.get::<bodyparser::Json>().unwrap().unwrap();

            let p = iexpect!(body.find("payload"));

            let str_payload = match *p {
                serde_json::Value::String(ref load) => load,
                _ => panic!("Unexpected payload type"),
            };

            str_payload.from_base64().expect("Payload is not base64")
        };

        let session: Uuid =
            itry!(iexpect!(req.extensions.get::<Router>().unwrap().find("session")).parse());


        let ref lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"));

        let username = "username";
        let password = "password";

        let id = Uuid::new_v4();

        let document = Document {
            id: id,
            payload: payload,
            version: 1,
        };


        match Handler::post(&SocketAddr::V4(context.node_addr),
                            &username,
                            &password,
                            document,
                            &session,
                            &LogId::from(lid).unwrap()) {
            Ok(id) => Ok(Response::with((status::Ok, format!("{}", id)))),
            Err(err) => {
                Ok(Response::with((status::InternalServerError,
                                   "An error occured when posting new document")))
            }
        }

    }

    fn http_delete(req: &mut Request, context: &Context) -> IronResult<Response> {
        let ref fileId = iexpect!(req.extensions
            .get::<Router>()
            .unwrap()
            .find("fileId"));

        let ref lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"));

        let username = "username";
        let password = "password";

        let session = Uuid::new_v4();

        let res = match Handler::remove(&SocketAddr::V4(context.node_addr),
                                        &username,
                                        &password,
                                        &Uuid::parse_str(*fileId).unwrap(),
                                        &session,
                                        &LogId::from(lid).unwrap()) {
            Ok(()) => Response::with((status::Ok, "Ok")),
            Err(err) => {
                Response::with((status::InternalServerError,
                                "An error occured when removing document"))
            }
        };

        Ok(res)
    }

    fn http_trans_delete(req: &mut Request, context: &Context) -> IronResult<Response> {
        let ref fileId = iexpect!(req.extensions
            .get::<Router>()
            .unwrap()
            .find("fileId"));

        let ref lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"));
        let session: Uuid =
            itry!(iexpect!(req.extensions.get::<Router>().unwrap().find("session")).parse());

        let username = "username";
        let password = "password";

        let res = match Handler::remove(&SocketAddr::V4(context.node_addr),
                                        &username,
                                        &password,
                                        &Uuid::parse_str(*fileId).unwrap(),
                                        &session,
                                        &LogId::from(lid).unwrap()) {
            Ok(()) => Response::with((status::Ok, "Ok")),
            Err(err) => {
                Response::with((status::InternalServerError,
                                "An error occured when removing document"))
            }
        };

        Ok(res)
    }

    fn http_put(req: &mut Request, context: &Context) -> IronResult<Response> {
        let payload = {
            let ref body = req.get::<bodyparser::Json>().unwrap().unwrap();

            let p = iexpect!(body.find("payload"));

            let str_payload = match *p {
                serde_json::Value::String(ref load) => load,
                _ => panic!("Unexpected payload type"),
            };

            str_payload.from_base64().expect("Payload is not base64")
        };

        let ref id = iexpect!(req.extensions.get::<Router>().unwrap().find("id"),
                              (status::BadRequest, "Cannot find id"));
        let ref lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"),
                               (status::BadRequest, "Cannot find logid"));

        let username = "username";
        let password = "password";

        let bytes = itry!(payload.from_base64(),
                          (status::BadRequest, "Payload is not base64"));

        let session = Uuid::new_v4();

        let res = match Handler::put(&SocketAddr::V4(context.node_addr),
                                     &username,
                                     &password,
                                     &Uuid::parse_str(&id).unwrap(),
                                     bytes,
                                     &session,
                                     &LogId::from(lid).unwrap()) {
            Ok(()) => Response::with((status::Ok, "Ok")),
            Err(err) => {
                Response::with((status::InternalServerError,
                                "An error occured when updating document"))
            }
        };
        Ok(res)

    }

    fn http_trans_put(req: &mut Request, context: &Context) -> IronResult<Response> {
        let payload = {
            let ref body = req.get::<bodyparser::Json>().unwrap().unwrap();

            let p = iexpect!(body.find("payload"));

            let str_payload = match *p {
                serde_json::Value::String(ref load) => load,
                _ => panic!("Unexpected payload type"),
            };

            itry!(str_payload.from_base64(),
                  (status::BadRequest, "Payload is not base64"))
        };

        let ref id = iexpect!(req.extensions.get::<Router>().unwrap().find("id"));
        let ref lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"));
        let session: Uuid =
            itry!(iexpect!(req.extensions.get::<Router>().unwrap().find("session")).parse());

        let username = "username";
        let password = "password";

        let res = match Handler::put(&SocketAddr::V4(context.node_addr),
                                     &username,
                                     &password,
                                     &Uuid::parse_str(&id).unwrap(),
                                     payload,
                                     &session,
                                     &LogId::from(lid).unwrap()) {
            Ok(()) => Response::with((status::Ok, "Ok")),
            Err(err) => {
                Response::with((status::InternalServerError,
                                "An error occured when updating document"))
            }
        };
        Ok(res)

    }

    fn http_begin_transaction(req: &mut Request, context: &Context) -> IronResult<Response> {
        let username = "username";
        let password = "password";
        let ref lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"),
                               (status::BadRequest, "Cannot find logid"));

        match Handler::begin_transaction(&SocketAddr::V4(context.node_addr),
                                         &username,
                                         &password,
                                         &Uuid::new_v4(),
                                         &LogId::from(lid).unwrap()) {
            Ok(session) => Ok(Response::with((status::Ok, session))),
            Err(_) => Ok(Response::with((status::InternalServerError, "Something went wrong :("))),
        }
    }

    fn http_commit_transaction(req: &mut Request, context: &Context) -> IronResult<Response> {
        let username = "username";
        let password = "password";
        let ref lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"),
                               (status::BadRequest, "Cannot find blogid"));

        match Handler::commit_transaction(&SocketAddr::V4(context.node_addr),
                                          &username,
                                          &password,
                                          &LogId::from(lid).unwrap()) {
            Ok(res) => Ok(Response::with((status::Ok, res))),
            Err(_) => Ok(Response::with((status::InternalServerError, "Something went wrong :("))),
        }
    }

    fn http_rollback_transaction(req: &mut Request, context: &Context) -> IronResult<Response> {
        let username = "username";
        let password = "password";
        let ref lid = iexpect!(req.extensions.get::<Router>().unwrap().find("lid"),
                               (status::BadRequest, "Cannot find logid"));

        match Handler::rollback_transaction(&SocketAddr::V4(context.node_addr),
                                            &username,
                                            &password,
                                            &LogId::from(lid).unwrap()) {
            Ok(res) => Ok(Response::with((status::Ok, res))),
            Err(_) => Ok(Response::with((status::InternalServerError, "Something went wrong :("))),
        }
    }
}
