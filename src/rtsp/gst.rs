//! This module provides an "RtspServer" abstraction that allows consumers of its API to feed it
//! data using an ordinary std::io::Write interface.
mod maybe_app_src;
mod maybe_inputselect;

pub(crate) use self::maybe_app_src::MaybeAppSrc;
pub(crate) use self::maybe_inputselect::MaybeInputSelect;

use super::state::States;
// use super::adpcm::adpcm_to_pcm;
// use super::errors::Error;
use gstreamer::prelude::Cast;
use gstreamer::{Bin, Structure};
use gstreamer_app::AppSrc;
//use gstreamer_rtsp::RTSPLowerTrans;
use anyhow::anyhow;
use gstreamer_rtsp::RTSPAuthMethod;
pub use gstreamer_rtsp_server::gio::{TlsAuthenticationMode, TlsCertificate};
use gstreamer_rtsp_server::glib;
use gstreamer_rtsp_server::prelude::*;
use gstreamer_rtsp_server::{
    RTSPAuth, RTSPMediaFactory, RTSPServer as GstRTSPServer, RTSPToken,
    RTSP_PERM_MEDIA_FACTORY_ACCESS, RTSP_PERM_MEDIA_FACTORY_CONSTRUCT,
    RTSP_TOKEN_MEDIA_FACTORY_ROLE,
};
use log::*;
use neolink_core::{
    bc_protocol::{Error, StreamOutput, StreamOutputError},
    bcmedia::model::*,
};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::io::Write;

type Result<T> = std::result::Result<T, ()>;
type AnyResult<T> = std::result::Result<T, anyhow::Error>;

pub(crate) struct RtspServer {
    server: GstRTSPServer,
}

#[derive(Eq, PartialEq, Debug)]
pub(crate) enum InputSources {
    Cam,
    TestSrc,
    Still,
    Black,
}

pub(crate) struct GstOutputs {
    pub(crate) audsrc: MaybeAppSrc,
    pub(crate) vidsrc: MaybeAppSrc,
    vid_inputselect: MaybeInputSelect,
    video_format: Option<StreamFormat>,
    audio_format: Option<StreamFormat>,
    factory: RTSPMediaFactory,
    state: Option<States>,
    last_iframe: Option<Vec<u8>>,
}

// The stream from the camera will be using one of these formats
//
// This is used as part of `StreamOutput` to give hints about
// the format of the stream
#[derive(Debug, PartialEq, Eq, Hash, Copy, Clone)]
enum StreamFormat {
    // H264 (AVC) video format
    H264,
    // H265 (HEVC) video format
    H265,
    // AAC audio
    Aac,
    // ADPCM in DVI-4 format
    Adpcm(u16),
}

impl StreamOutput for GstOutputs {
    fn stream_recv(&mut self, media: BcMedia) -> StreamOutputError {
        let mut should_continue = true;

        // Ensure stream is on cam mode
        self.set_input_source(InputSources::Cam).map_err(|e| {
            Error::OtherString(format!("Cannot set stream input to camera source {:?}", e))
        })?;

        match media {
            BcMedia::Iframe(payload) => {
                let video_type = match payload.video_type {
                    VideoType::H264 => StreamFormat::H264,
                    VideoType::H265 => StreamFormat::H265,
                };
                self.set_format(Some(video_type));
                self.vidsrc.write_all(&payload.data).map_err(|e| {
                    Error::OtherString(format!("Cannot write IFrame to vidsrc {:?}", e))
                })?;
                self.last_iframe = Some(payload.data);
                // Only stop on an iframe so we have the
                // last frame to show
                if let Some(state) = self.state.as_ref() {
                    should_continue = state.should_stream()
                }
            }
            BcMedia::Pframe(payload) => {
                let video_type = match payload.video_type {
                    VideoType::H264 => StreamFormat::H264,
                    VideoType::H265 => StreamFormat::H265,
                };
                self.set_format(Some(video_type));
                self.vidsrc.write_all(&payload.data).map_err(|e| {
                    Error::OtherString(format!("Cannot write PFrame to vidsrc {:?}", e))
                })?;
            }
            BcMedia::Aac(payload) => {
                self.set_format(Some(StreamFormat::Aac));
                self.audsrc.write_all(&payload.data).map_err(|e| {
                    Error::OtherString(format!("Cannot write AAC to audsrc {:?}", e))
                })?;
            }
            BcMedia::Adpcm(payload) => {
                self.set_format(Some(StreamFormat::Adpcm(payload.data.len() as u16)));
                self.audsrc.write_all(&payload.data).map_err(|e| {
                    Error::OtherString(format!("Cannot write ADPCM to audsrc {:?}", e))
                })?;
            }
            _ => {
                //Ignore other BcMedia like InfoV1 and InfoV2
            }
        }

        Ok(should_continue)
    }
}

impl GstOutputs {
    pub(crate) fn from_appsrcs(
        vidsrc: MaybeAppSrc,
        audsrc: MaybeAppSrc,
        vid_inputselect: MaybeInputSelect,
    ) -> GstOutputs {
        let result = GstOutputs {
            vidsrc,
            audsrc,
            vid_inputselect,
            video_format: None,
            audio_format: None,
            factory: RTSPMediaFactory::new(),
            last_iframe: Default::default(),
            state: Default::default(),
        };
        result.apply_format();
        result
    }

    pub(crate) fn set_state(&mut self, state: States) {
        self.vidsrc.state = Some(state.clone());
        self.state = Some(state)
    }

    pub(crate) fn set_input_source(&mut self, input_source: InputSources) -> AnyResult<()> {
        match &input_source {
            InputSources::Cam => self.vid_inputselect.set_input(0),
            InputSources::TestSrc => self.vid_inputselect.set_input(1),
            InputSources::Still => self.vid_inputselect.set_input(2),
            InputSources::Black => self.vid_inputselect.set_input(3),
        }
    }

    pub(crate) fn has_last_iframe(&self) -> bool {
        self.last_iframe.is_some()
    }

    pub(crate) fn write_last_iframe(&mut self) -> AnyResult<()> {
        self.vidsrc.write_all(
            self.last_iframe
                .as_ref()
                .ok_or_else(|| anyhow!("No iframe data avaliable"))?
                .as_slice(),
        )?;
        Ok(())
    }

    fn set_format(&mut self, format: Option<StreamFormat>) {
        match format {
            Some(StreamFormat::H264) | Some(StreamFormat::H265) => {
                if format != self.video_format {
                    self.video_format = format;
                    self.apply_format();
                }
            }
            Some(StreamFormat::Aac) | Some(StreamFormat::Adpcm(_)) => {
                if format != self.audio_format {
                    self.audio_format = format;
                    self.apply_format();
                }
            }
            _ => {}
        }
    }

    fn apply_format(&self) {
        let launch_vid_select = match self.video_format {
            Some(StreamFormat::H264) => "! rtph264pay name=pay0",
            Some(StreamFormat::H265) => "! rtph265pay name=pay0",
            _ => "! fakesink",
        };

        let launch_app_src = match self.video_format {
            Some(StreamFormat::H264) => {
                "! queue silent=true max-size-bytes=10485760  min-threshold-bytes=1024 ! h264parse"
            }
            Some(StreamFormat::H265) => {
                "! queue silent=true  max-size-bytes=10485760  min-threshold-bytes=1024 ! h265parse"
            }
            _ => "",
        };

        let launch_alt_source = match self.video_format {
            Some(StreamFormat::H264) => {
                "videotestsrc ! queue silent=true  max-size-bytes=10485760  min-threshold-bytes=1024 ! x264enc ! video/x-h264,width=896,height=512,framerate=25/1 ! h264parse"
            }
            Some(StreamFormat::H265) => {
                "videotestsrc ! queue silent=true  max-size-bytes=10485760  min-threshold-bytes=1024 ! x265enc ! video/x-h265,width=896,height=512,framerate=25/1 ! h265parse"
            }
            _ => "",
        };

        let launch_imgfreeze_source = match self.video_format {
            Some(StreamFormat::H264) => {
                "avdec_h264 ! imagefreeze allow-replace=true is-live=true ! x264enc ! video/x-h264,width=896,height=512,framerate=25/1 ! h264parse"
            }
            Some(StreamFormat::H265) => {
                "decodebin ! imagefreeze allow-replace=true is-live=true ! x265enc ! video/x-h265,width=896,height=512,framerate=25/1 ! h265parse"
            }
            _ => "",
        };

        let launch_aud = match self.audio_format {
            Some(StreamFormat::Adpcm(block_size)) => format!("caps=audio/x-adpcm,layout=dvi,block_align={},channels=1,rate=8000 ! queue silent=true max-size-bytes=10485760 min-threshold-bytes=1024 ! adpcmdec  ! audioconvert ! rtpL16pay name=pay1", block_size), // DVI4 is converted to pcm in the appsrc
            Some(StreamFormat::Aac) => "! queue silent=true max-size-bytes=10485760 min-threshold-bytes=1024 ! aacparse ! decodebin ! audioconvert ! rtpL16pay name=pay1 name=pay1".to_string(),
            _ => "! fakesink".to_string(),
        };

        let launch_str = &vec![
            "( ",
                // Video out pipe
                "(",
                    "input-selector name=vid_inputselect",
                    launch_vid_select,
                ")",
                // Camera vid source
                "(",
                    "appsrc name=vidsrc is-live=true block=true emit-signals=false max-bytes=52428800 do-timestamp=true format=GST_FORMAT_TIME", // 50MB max size so that it won't grow to infinite if the queue blocks
                    launch_app_src,
                    "! tee name=vid_src_tee",
                    "(",
                        "vid_src_tee. ! queue silent=true  max-size-bytes=10485760  min-threshold-bytes=1024 ! vid_inputselect.sink_0",
                    ")",

                ")",
                // Test vid source
                "(",
                    launch_alt_source,
                    "! vid_inputselect.sink_1",
                ")",
                // Image freeze
                "(",
                    "vid_src_tee. ! queue silent=true  max-size-bytes=10485760  min-threshold-bytes=1024 !",
                    launch_imgfreeze_source,
                    " ! vid_inputselect.sink_2",
                ")",
                // Audio pipe
                "appsrc name=audsrc is-live=true block=true emit-signals=false max-bytes=52428800 do-timestamp=true format=GST_FORMAT_TIME",
                &launch_aud,
            ")"
        ]
        .join(" ");
        debug!("Gstreamer launch str: {:?}", launch_str);
        self.factory.set_launch(launch_str);
    }
}

impl Default for RtspServer {
    fn default() -> RtspServer {
        Self::new()
    }
}

impl RtspServer {
    pub(crate) fn new() -> RtspServer {
        gstreamer::init().expect("Gstreamer should not explode");
        RtspServer {
            server: GstRTSPServer::new(),
        }
    }

    pub(crate) fn add_stream(
        &self,
        paths: &[&str],
        permitted_users: &HashSet<&str>,
    ) -> Result<GstOutputs> {
        let mounts = self
            .server
            .mount_points()
            .expect("The server should have mountpoints");

        // Create a MaybeAppSrc: Write which we will give the caller.  When the backing AppSrc is
        // created by the factory, fish it out and give it to the waiting MaybeAppSrc via the
        // channel it provided.  This callback may be called more than once by Gstreamer if it is
        // unhappy with the pipeline, so keep updating the MaybeAppSrc.
        let (maybe_app_src, tx) = MaybeAppSrc::new_with_tx();
        let (maybe_app_src_aud, tx_aud) = MaybeAppSrc::new_with_tx();

        let (maybe_vid_inputselect, tx_vid_inputselect) = MaybeInputSelect::new_with_tx();

        let outputs =
            GstOutputs::from_appsrcs(maybe_app_src, maybe_app_src_aud, maybe_vid_inputselect);

        let factory = &outputs.factory;

        debug!(
            "Permitting {} to access {}",
            // This is hashmap or (iter) equivalent of join, it requres itertools
            itertools::Itertools::intersperse(permitted_users.iter().cloned(), ", ")
                .collect::<String>(),
            paths.join(", ")
        );
        self.add_permitted_roles(factory, permitted_users);

        factory.set_shared(true);

        factory.connect_media_configure(move |_factory, media| {
            debug!("RTSP: media was configured");
            let bin = media
                .element()
                .expect("Media should have an element")
                .dynamic_cast::<Bin>()
                .expect("Media source's element should be a bin");
            let app_src = bin
                .by_name_recurse_up("vidsrc")
                .expect("write_src must be present in created bin")
                .dynamic_cast::<AppSrc>()
                .expect("Source element is expected to be an appsrc!");
            let _ = tx.send(app_src); // Receiver may be dropped, don't panic if so

            let app_src_aud = bin
                .by_name_recurse_up("audsrc")
                .expect("write_src must be present in created bin")
                .dynamic_cast::<AppSrc>()
                .expect("Source element is expected to be an appsrc!");
            let _ = tx_aud.send(app_src_aud); // Receiver may be dropped, don't panic if so

            let maybe_vid_inputselect = bin
                .by_name_recurse_up("vid_inputselect")
                .expect("vid_inputselect must be present in created bin");
            let _ = tx_vid_inputselect.send(maybe_vid_inputselect); // Receiver may be dropped, don't panic if so
        });

        for path in paths {
            mounts.add_factory(path, factory);
        }

        Ok(outputs)
    }

    pub(crate) fn add_permitted_roles(
        &self,
        factory: &RTSPMediaFactory,
        permitted_roles: &HashSet<&str>,
    ) {
        for permitted_role in permitted_roles {
            factory.add_role_from_structure(&Structure::new(
                permitted_role,
                &[
                    (*RTSP_PERM_MEDIA_FACTORY_ACCESS, &true),
                    (*RTSP_PERM_MEDIA_FACTORY_CONSTRUCT, &true),
                ],
            ));
        }
        // During auth, first it binds anonymously. At this point it checks
        // RTSP_PERM_MEDIA_FACTORY_ACCESS to see if anyone can connect
        // This is done before the auth token is loaded, possibliy an upstream bug there
        // After checking RTSP_PERM_MEDIA_FACTORY_ACCESS anonymously
        // It loads the auth token of the user and checks that users
        // RTSP_PERM_MEDIA_FACTORY_CONSTRUCT allowing them to play
        // As a result of this we must ensure that if anonymous is not granted RTSP_PERM_MEDIA_FACTORY_ACCESS
        // As a part of permitted users then we must allow it to access
        // at least RTSP_PERM_MEDIA_FACTORY_ACCESS but not RTSP_PERM_MEDIA_FACTORY_CONSTRUCT
        // Watching Actually happens during RTSP_PERM_MEDIA_FACTORY_CONSTRUCT
        // So this should be OK to do.
        // FYI: If no RTSP_PERM_MEDIA_FACTORY_ACCESS then server returns 404 not found
        //      If yes RTSP_PERM_MEDIA_FACTORY_ACCESS but no RTSP_PERM_MEDIA_FACTORY_CONSTRUCT
        //        server returns 401 not authourised
        if !permitted_roles.contains(&"anonymous") {
            factory.add_role_from_structure(&Structure::new(
                "anonymous",
                &[(*RTSP_PERM_MEDIA_FACTORY_ACCESS, &true)],
            ));
        }
    }

    pub(crate) fn set_credentials(&self, credentials: &[(&str, &str)]) -> Result<()> {
        let auth = self.server.auth().unwrap_or_else(RTSPAuth::new);
        auth.set_supported_methods(RTSPAuthMethod::Basic);

        let mut un_authtoken = RTSPToken::new(&[(*RTSP_TOKEN_MEDIA_FACTORY_ROLE, &"anonymous")]);
        auth.set_default_token(Some(&mut un_authtoken));

        for credential in credentials {
            let (user, pass) = credential;
            trace!("Setting credentials for user {}", user);
            let token = RTSPToken::new(&[(*RTSP_TOKEN_MEDIA_FACTORY_ROLE, user)]);
            let basic = RTSPAuth::make_basic(user, pass);
            auth.add_basic(basic.as_str(), &token);
        }

        self.server.set_auth(Some(&auth));
        Ok(())
    }

    pub(crate) fn set_tls(
        &self,
        cert_file: &str,
        client_auth: TlsAuthenticationMode,
    ) -> Result<()> {
        debug!("Setting up TLS using {}", cert_file);
        let auth = self.server.auth().unwrap_or_else(RTSPAuth::new);

        // We seperate reading the file and changing to a PEM so that we get different error messages.
        let cert_contents = fs::read_to_string(cert_file).expect("TLS file not found");
        let cert = TlsCertificate::from_pem(&cert_contents).expect("Not a valid TLS certificate");
        auth.set_tls_certificate(Some(&cert));
        auth.set_tls_authentication_mode(client_auth);

        self.server.set_auth(Some(&auth));
        Ok(())
    }

    pub(crate) fn run(&self, bind_addr: &str, bind_port: u16) {
        self.server.set_address(bind_addr);
        self.server.set_service(&format!("{}", bind_port));
        // Attach server to default Glib context
        let _ = self.server.attach(None);

        // Run the Glib main loop.
        let main_loop = glib::MainLoop::new(None, false);
        main_loop.run();
    }
}
