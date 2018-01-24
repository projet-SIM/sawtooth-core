/*
 * Copyright 2017 Bitwise IO, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 * -----------------------------------------------------------------------------
 */

extern crate zmq;
extern crate protobuf;
extern crate rand;

use self::rand::Rng;

pub mod handler;

use std::error::Error;

use protobuf::Message as M;
use protobuf::repeated::RepeatedField;
use messages::validator::Message_MessageType;
use messages::processor::TpRegisterRequest;
use messages::processor::TpProcessRequest;
use messages::processor::TpProcessResponse;
use messages::processor::TpProcessResponse_Status;
use messaging::stream::MessageConnection;
use messaging::stream::MessageSender;
use messaging::zmq_stream::ZmqMessageSender;
use messaging::stream::SendError;
use messaging::stream::ReceiveError;
use messaging::zmq_stream::ZmqMessageConnection;

use self::handler::TransactionContext;
use self::handler::TransactionHandler;
use self::handler::ApplyError;

/// Generates a random correlation id for use in Message
fn generate_correlation_id() -> String {
    const LENGTH: usize = 16;
    rand::thread_rng().gen_ascii_chars().take(LENGTH).collect()
}

pub struct TransactionProcessor<'a> {
    endpoint: String,
    conn: ZmqMessageConnection,
    handlers: Vec<&'a TransactionHandler>
}

impl<'a> TransactionProcessor<'a> {
    /// TransactionProcessor is for communicating with a
    /// validator and routing transaction processing requests to a registered
    /// handler. It uses ZMQ and channels to handle requests concurrently.
    pub fn new(endpoint: &str) -> TransactionProcessor {
        TransactionProcessor {
            endpoint: String::from(endpoint),
            conn: ZmqMessageConnection::new(endpoint),
            handlers: Vec::new()
        }
    }

    /// Adds a transaction family handler
    ///
    /// # Arguments
    ///
    /// * handler - the handler to be added
    pub fn add_handler(&mut self, handler: &'a TransactionHandler) {
        self.handlers.push(handler);
    }

    fn register(&mut self, mut sender: ZmqMessageSender) -> bool {
        for handler in &self.handlers {
            for version in handler.family_versions() {
                let mut request = TpRegisterRequest::new();
                request.set_family(handler.family_name().clone());
                request.set_version(version.clone());
                request.set_namespaces(RepeatedField::from_vec(handler.namespaces().clone()));
                info!("sending TpRegisterRequest: {} {}",
                      &handler.family_name(),
                      &version);
                let serialized = match request.write_to_bytes() {
                    Ok(serialized) => serialized,
                    Err(err) => {
                        error!("Serialization failed: {}", err.description());
                        // try reconnect
                        return false
                    }
                };
                let x : &[u8] = &serialized;

                let mut future = match sender.send(
                    Message_MessageType::TP_REGISTER_REQUEST,
                    &generate_correlation_id(),
                    x) {
                        Ok(fut) => fut,
                        Err(err) => {
                            error!("Registration failed: {}", err.description());
                            // try reconnect
                            return false
                        }
                    };

                // Absorb the TpRegisterResponse message
                let _ = match future.get(){
                    Ok(_) => (),
                    Err(err) => {
                        error!("Registration failed: {}", err.description());
                        // try reconnect
                        return false
                    }
                };
            }
        }
        true
    }

    /// Connects the transaction processor to a validator and starts
    /// listening for requests and routing them to an appropriate
    /// transaction handler.
    pub fn start(&mut self) {
        let mut first_time = true;
        let mut restart = true;
        while restart {
            info!("connecting to endpoint: {}", self.endpoint);
            if first_time {
                first_time = false;
            } else {
                self.conn = ZmqMessageConnection::new(&self.endpoint);
            }
            let (mut sender, receiver) = self.conn.create();
            // if registration is not succesful, retry
            match self.register(sender.clone()) {
                true => (),
                false => continue
            }

            loop {
                match receiver.recv() {
                    Ok(r) => {
                        // Check if we have a message
                        let message = match r {
                            Ok(message) => message,
                            Err(ReceiveError::DisconnectedError)=> {
                                info!("Trying to Reconnect");
                                break;
                            }
                            Err(err) => {
                                error!("Error: {}", err.description());
                                continue;
                            }
                        };

                        info!("Message: {}", message.get_correlation_id());

                        match message.get_message_type() {
                            Message_MessageType::TP_PROCESS_REQUEST => {
                                let request: TpProcessRequest = match protobuf::parse_from_bytes(
                                    &message.get_content()) {
                                    Ok(request) => request,
                                    Err(err) => {
                                        error!("Cannot parse TpProcessRequest: {}",
                                               err.description());
                                        continue
                                    }
                                };

                                let mut context = TransactionContext::new(
                                    request.get_context_id(), sender.clone());

                                let mut response = TpProcessResponse::new();
                                match self.handlers[0].apply(&request, &mut context) {
                                    Ok(()) => {
                                        response.set_status(TpProcessResponse_Status::OK);
                                        info!("TP_PROCESS_REQUEST sending TpProcessResponse: OK");
                                    },
                                    Err(ApplyError::InvalidTransaction(msg)) => {
                                        response.set_status(
                                            TpProcessResponse_Status::INVALID_TRANSACTION);
                                        response.set_message(msg.clone());
                                        info!("TP_PROCESS_REQUEST sending TpProcessResponse: {}",
                                              msg);
                                    },
                                    Err(err) => {
                                        response.set_status(
                                            TpProcessResponse_Status::INTERNAL_ERROR);
                                        response.set_message(String::from(err.description()));
                                        info!("TP_PROCESS_REQUEST sending TpProcessResponse: {}",
                                              err.description());
                                    }
                                };

                                let serialized = match response.write_to_bytes()
                                {
                                    Ok(serialized) => serialized,
                                    Err(err) => {
                                        error!("Serialization failed: {}", err.description());
                                        continue
                                    }
                                };

                                let x : &[u8] = &serialized;
                                match sender.reply(
                                    Message_MessageType::TP_PROCESS_RESPONSE,
                                    message.get_correlation_id(),
                                    x) {
                                        Ok(_) => (),
                                        Err(SendError::DisconnectedError) => {
                                            error!("DisconnectedError");
                                            break
                                        },
                                        Err(SendError::TimeoutError) =>
                                            error!("TimeoutError"),
                                        Err(SendError::UnknownError) => {
                                            restart = false;
                                            println!("UnknownError");
                                            break
                                        }
                                    };
                            },
                            _ => {
                            let mut response = TpProcessResponse::new();
                            response.set_status(TpProcessResponse_Status::INTERNAL_ERROR);
                            response.set_message(String::from("not implemented..."));
                            let serialized = response.write_to_bytes().unwrap();
                            let x : &[u8] = &serialized;
                            match sender.reply(
                                Message_MessageType::TP_PROCESS_RESPONSE,
                                message.get_correlation_id(),
                                x){
                                    Ok(_) => (),
                                    Err(SendError::DisconnectedError) => {
                                        error!("DisconnectedError");
                                        break
                                    },
                                    Err(SendError::TimeoutError) =>
                                        error!("TimeoutError"),
                                    Err(SendError::UnknownError) => {
                                        restart = false;
                                        println!("UnknownError");
                                        break
                                    }
                                };
                            }
                        }
                    }
                    Err(err) => {
                        error!("Error: {}", err.description());
                    }
                }
            }
            sender.close();
        }
    }
}