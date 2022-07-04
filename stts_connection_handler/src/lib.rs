//! Connection handler for STTS.

#[macro_use]
extern crate tracing;

use byteorder::{ByteOrder, NetworkEndian, ReadBytesExt, WriteBytesExt};
use std::fmt::Write as _;
use std::io;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use stts_speech_to_text::{Error, Stream};

pub struct ConnectionHandler {
    stream: UnixStream,
    model: Option<Stream>,
    verbose: bool,
}

impl From<UnixStream> for ConnectionHandler {
    fn from(stream: UnixStream) -> Self {
        Self {
            stream,
            model: None,
            verbose: false,
        }
    }
}

impl ConnectionHandler {
    /// Enter the main loop of the connection handler.
    pub fn handle(&mut self) {
        loop {
            debug!("waiting for command");
            // read the type of the next incoming message
            let t = match self.stream.read_u8() {
                Ok(t) => t,
                Err(e) => {
                    error!("error reading message type: {}", e);
                    let _ = self.stream.write_u8(0xFD);
                    let _ = write_string(&mut self.stream, &e.to_string());
                    break;
                }
            };
            debug!("received command: {:02x}", t);
            let res = match t {
                0x00 => self.handle_0x00(),
                0x01 => self.handle_0x01(),
                0x02 => self.handle_0x02(),
                0x03 => self.handle_0x03(),
                _ => {
                    warn!("unknown message type: {}", t);
                    let _ = self.stream.write_u8(0xFE);
                    break;
                }
            };
            debug!("handled command: {:02x}", t);

            match res {
                Ok(e) if e => break,
                Err(e) => {
                    error!("error writing message: {}", e);
                    let _ = self.stream.write_u8(0xFD);
                    let _ = write_string(&mut self.stream, &e.to_string());
                    break;
                }
                _ => {}
            };
        }
        debug!("exiting");
        // shutdown the connection
        if let Err(e) = self.stream.shutdown(Shutdown::Both) {
            error!("error shutting down connection: {}", e);
        }
    }

    fn handle_0x00(&mut self) -> io::Result<bool> {
        // 0x00: Initialize Streaming

        // field 0: verbose: bool
        // read as a u8, then convert to bool
        trace!("reading verbose");
        let verbose = self.stream.read_u8()? != 0;

        // field 1: language: String
        trace!("reading language");
        let language = read_string(&mut self.stream)?;

        debug!("loading stream");
        let retval = match stts_speech_to_text::get_stream(&language) {
            Some(stream) => {
                debug!("loaded stream");
                self.model = Some(stream);
                self.verbose = verbose;
                // success!
                self.stream.write_u8(0x00)?;
                false
            }
            None => {
                warn!("failed to load stream");
                // error!
                self.stream.write_u8(0xFE)?;
                true
            }
        };
        Ok(retval)
    }

    fn handle_0x01(&mut self) -> io::Result<bool> {
        // 0x01: Audio Data;

        // field 0: data_len: u32
        trace!("reading data length");
        let data_len = self.stream.read_u32::<NetworkEndian>()?;
        trace!("need to read {} bytes", data_len);

        // field 1: data: Vec<i16>, of length data_len/2, in NetworkEndian order
        trace!("reading data");
        let mut buf = vec![0; data_len as usize];
        self.stream.read_exact(&mut buf)?;

        // fetch the model, if it doesn't exist, return and ignore this message
        trace!("fetching model");
        let model = match self.model {
            Some(ref mut model) => model,
            None => return Ok(false),
        };

        // if the model exists, *then* spend the handful of CPU cycles to process the audio data
        trace!("processing audio data");
        let mut data = vec![0; (data_len / 2) as usize];
        byteorder::NetworkEndian::read_i16_into(&buf, &mut data);
        trace!("found {} samples", data.len());

        debug!("feeding data");
        // feed the audio data to the model
        model.feed_audio(&data);

        Ok(false)
    }

    fn handle_0x02(&mut self) -> io::Result<bool> {
        // 0x02: Finalize Streaming

        // no fields

        // fetch the model, if it doesn't exist, return and ignore this message
        trace!("fetching model");
        let model = match self.model.take() {
            Some(model) => model,
            None => return Ok(false),
        };

        if self.verbose {
            debug!("finalizing model");
            match model.finish_stream_with_metadata(3) {
                Ok(r) => {
                    trace!("writing header");
                    self.stream.write_u8(0x03)?;
                    let num = r.num_transcripts();
                    trace!("writing num_transcripts");
                    self.stream.write_u32::<NetworkEndian>(num)?;
                    if num != 0 {
                        let transcripts = r.transcripts();
                        let main_transcript = unsafe { transcripts.get_unchecked(0) };
                        let tokens = main_transcript.tokens();
                        let mut res = String::new();
                        for token in tokens {
                            res.write_str(token.text().as_ref())
                                .expect("error writing to string");
                        }
                        trace!("writing transcript");
                        write_string(&mut self.stream, &res)?;
                        trace!("writing confidence");
                        self.stream
                            .write_f64::<NetworkEndian>(main_transcript.confidence())?;
                    }
                }
                Err(e) => {
                    warn!("error finalizing model: {}", e);
                    trace!("writing header");
                    self.stream.write_u8(0x04)?;
                    let num_err = conv_err(e);
                    trace!("writing error");
                    self.stream.write_i64::<NetworkEndian>(num_err)?;
                }
            }
        } else {
            debug!("finalizing model");
            match model.finish_stream() {
                Ok(s) => {
                    trace!("writing header");
                    self.stream.write_u8(0x02)?;
                    trace!("writing transcript");
                    write_string(&mut self.stream, &s)?;
                }
                Err(e) => {
                    warn!("error finalizing model: {}", e);
                    trace!("writing header");
                    self.stream.write_u8(0x04)?;
                    let num_err = conv_err(e);
                    trace!("writing error");
                    self.stream.write_i64::<NetworkEndian>(num_err)?;
                }
            }
        }

        Ok(true)
    }

    fn handle_0x03(&mut self) -> io::Result<bool> {
        // 0x03: Close Connection

        // no fields

        // immediately close the connection
        Ok(true)
    }
}

fn read_string(stream: &mut UnixStream) -> io::Result<String> {
    // strings are encoded as a u64 length followed by the string bytes
    let len = stream.read_u64::<NetworkEndian>()?;
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).to_string())
}

fn write_string(stream: &mut UnixStream, string: &str) -> io::Result<()> {
    // strings are encoded as a u64 length followed by the string bytes
    // cache the bytes to prevent a second call to .as_bytes()
    let bytes = string.as_bytes();
    let len = bytes.len() as u64;
    stream.write_u64::<NetworkEndian>(len as u64)?;
    stream.write_all(bytes)?;
    Ok(())
}

fn conv_err(e: Error) -> i64 {
    match e {
        Error::NoModel => 2147483649,
        Error::InvalidAlphabet => 0x2000,
        Error::InvalidShape => 0x2001,
        Error::InvalidScorer => 0x2002,
        Error::ModelIncompatible => 0x2003,
        Error::ScorerNotEnabled => 0x2004,
        Error::ScorerUnreadable => 0x2005,
        Error::ScorerInvalidHeader => 0x2006,
        Error::ScorerNoTrie => 0x2007,
        Error::ScorerInvalidTrie => 0x2008,
        Error::ScorerVersionMismatch => 0x2009,
        Error::InitMmapFailed => 0x3000,
        Error::InitSessionFailed => 0x3001,
        Error::InterpreterFailed => 0x3002,
        Error::RunSessionFailed => 0x3003,
        Error::CreateStreamFailed => 0x3004,
        Error::ReadProtoBufFailed => 0x3005,
        Error::CreateSessionFailed => 0x3006,
        Error::CreateModelFailed => 0x3007,
        Error::InsertHotWordFailed => 0x3008,
        Error::ClearHotWordsFailed => 0x3009,
        Error::EraseHotWordFailed => 0x3010,
        Error::Other(n) => n as i64,
        Error::Unknown => 2147483650,
        Error::NulBytesFound => 2147483651,
        Error::Utf8Error(_) => 2147483652,
        _ => i64::MIN,
    }
}
