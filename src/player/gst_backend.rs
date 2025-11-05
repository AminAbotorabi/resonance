/* gst_backend.rs
 *
 * SPDX-FileCopyrightText: 2023 nate-xyz
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * Rust rewrite of gstplayer.py from GNOME Music (GPLv2)
 * used my own python rewrite of gnome music gstplayer, Noteworthy rewrite, amberol gst backend for reference
 * See https://gitlab.gnome.org/GNOME/gnome-music/-/blob/master/gnomemusic/gstplayer.py
 * See https://github.com/SeaDve/Noteworthy/blob/main/src/core/audio_player.rs
 * See https://gitlab.gnome.org/World/amberol/-/blob/main/src/audio/gst_backend.rs
 *
 */

use gst::{glib::Continue, prelude::*};
use gtk::{glib, glib::clone, glib::Sender};
use gtk_macros::send;

use log::{debug, error};
use std::time::Duration;
use std::{cell::Cell, cell::RefCell, rc::Rc};

use super::player::PlaybackAction;

#[derive(Debug, Clone, Copy, PartialEq, glib::Enum)]
#[enum_type(name = "GstPlayerPlaybackState")]
pub enum BackendPlaybackState {
    Stopped,
    Loading,
    Paused,
    Playing,
}

impl Default for BackendPlaybackState {
    fn default() -> Self {
        Self::Stopped
    }
}

#[derive(Debug)]
pub struct GstPlayer {
    pub sender: Sender<PlaybackAction>,
    pub pipeline: gst::Pipeline,
    pub pipeline_next: gst::Pipeline, // Second pipeline for crossfade
    pub active_pipeline: Cell<u8>,    // 0 = primary, 1 = secondary
    pub state: Cell<BackendPlaybackState>,
    pub clock_id: RefCell<Option<gst::PeriodicClockId>>,
    pub clock: RefCell<Option<gst::Clock>>,
    // pub tick: Cell<u64>,
    pub duration: RefCell<Option<f64>>,
    pub volume: Cell<f64>,
    pub crossfade_duration: Cell<f64>, // Crossfade duration in seconds
    pub is_crossfading: Cell<bool>,
}

impl GstPlayer {
    pub fn new(player_sender: Sender<PlaybackAction>) -> Rc<GstPlayer> {
        let pipeline = gst::ElementFactory::make_with_name("playbin3", None)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();

        let pipeline_next = gst::ElementFactory::make_with_name("playbin3", None)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();

        let gstplayer = Rc::new(Self {
            sender: player_sender,
            pipeline,
            pipeline_next,
            active_pipeline: Cell::new(0),
            state: Cell::new(BackendPlaybackState::default()),
            clock_id: RefCell::new(None),
            clock: RefCell::new(None),
            // tick: Cell::new(0),
            duration: RefCell::new(None),
            volume: Cell::new(0.0),
            crossfade_duration: Cell::new(3.0),
            is_crossfading: Cell::new(false),
        });

        gstplayer.clone().connect_bus();
        gstplayer.clone().connect_bus_next();
        gstplayer
    }

    pub fn pipeline(&self) -> &gst::Pipeline {
        if self.active_pipeline.get() == 0 {
            &self.pipeline
        } else {
            &self.pipeline_next
        }
    }

    pub fn other_pipeline(&self) -> &gst::Pipeline {
        if self.active_pipeline.get() == 0 {
            &self.pipeline_next
        } else {
            &self.pipeline
        }
    }

    // STATE
    pub fn set_state(&self, state: BackendPlaybackState) {
        if let Err(e) = self.set_pipeline_gst_state(state) {
            error!("setting backend state error: {}", e)
        } else {
            // Have to send manually bc NULL flushes the pipeline
            if state == BackendPlaybackState::Stopped {
                self.state.set(state);
                send!(self.sender, PlaybackAction::PlaybackState(state));
            }
        }
    }

    pub fn set_pipeline_gst_state(
        &self,
        state: BackendPlaybackState,
    ) -> Result<(), gst::StateChangeError> {
        match state {
            BackendPlaybackState::Paused => {
                self.pipeline().set_state(gst::State::Paused)?;
                let _ = self.other_pipeline().set_state(gst::State::Paused);
            }
            BackendPlaybackState::Stopped => {
                // Changing the state to NULL flushes the pipeline.
                // Thus, the change message never arrives.
                self.pipeline.set_state(gst::State::Null)?;
                self.pipeline_next.set_state(gst::State::Null)?;
            }
            BackendPlaybackState::Loading => {
                //debug!("setting ready");
                self.pipeline().set_state(gst::State::Ready)?;
            }
            BackendPlaybackState::Playing => {
                //debug!("setting playing");
                self.pipeline().set_state(gst::State::Playing)?;
            }
        }
        Ok(())
    }

    pub fn state(&self) -> BackendPlaybackState {
        self.state.get()
    }

    // URI
    pub fn set_uri(&self, uri: String) {
        let uri_encoded = urlencoding::encode(&uri);
        let replaced = uri_encoded.replace("%2F", "/");
        self.pipeline()
            .set_property("uri", format!("file:{}", replaced).to_value());
    }

    pub fn set_uri_next(&self, uri: String) {
        let uri_encoded = urlencoding::encode(&uri);
        let replaced = uri_encoded.replace("%2F", "/");
        self.other_pipeline()
            .set_property("uri", format!("file:{}", replaced).to_value());
    }

    pub fn set_crossfade_duration(&self, duration: f64) {
        self.crossfade_duration.set(duration.clamp(0.0, 12.0));
    }

    pub fn crossfade_duration(&self) -> f64 {
        self.crossfade_duration.get()
    }

    //VOLUME
    pub fn set_volume(&self, volume: f64) {
        let mut set_volume = volume.clamp(0.0, 1.0);
        if set_volume <= 0.05 {
            set_volume = 0.0;
        }

        let linear_volume = gst_audio::StreamVolume::convert_volume(
            gst_audio::StreamVolumeFormat::Cubic,
            gst_audio::StreamVolumeFormat::Linear,
            set_volume,
        );
        self.pipeline
            .set_property_from_value("volume", &linear_volume.to_value());
        self.pipeline_next
            .set_property_from_value("volume", &linear_volume.to_value());
        self.volume.set(linear_volume);
    }

    fn set_volume_internal(&self) {
        self.pipeline
            .set_property_from_value("volume", &self.volume.get().to_value());
        self.pipeline_next
            .set_property_from_value("volume", &self.volume.get().to_value());
    }

    fn set_volume_for_pipeline(&self, pipeline: &gst::Pipeline, volume: f64) {
        let linear_volume = volume.clamp(0.0, 1.0);
        pipeline.set_property_from_value("volume", &linear_volume.to_value());
    }

    pub fn volume(&self) -> f64 {
        self.pipeline().property("volume")
    }

    //POSITION
    pub fn pipeline_position_in_nsecs(&self) -> Option<u64> {
        let active_pipeline = self.pipeline();
        let pos: Option<gst::ClockTime> = {
            // Create a new position query and send it to the pipeline.
            // This will traverse all elements in the pipeline, until one feels
            // capable of answering the query.
            let mut q = gst::query::Position::new(gst::Format::Time);
            if active_pipeline.query(&mut q) {
                Some(q.result())
            } else {
                None
            }
        }
        .and_then(|pos| pos.try_into().ok())?;
        match pos {
            Some(d) => Some(d.nseconds()),
            None => None,
        }
    }

    pub fn pipeline_position(&self) -> Option<u64> {
        let active_pipeline = self.pipeline();
        let pos: Option<gst::ClockTime> = {
            // Create a new position query and send it to the pipeline.
            // This will traverse all elements in the pipeline, until one feels
            // capable of answering the query.
            let mut q = gst::query::Position::new(gst::Format::Time);
            if active_pipeline.query(&mut q) {
                Some(q.result())
            } else {
                None
            }
        }
        .and_then(|pos| pos.try_into().ok())?;
        match pos {
            Some(d) => Some(d.seconds()),
            None => None,
        }
    }

    //DURATION
    pub fn pipeline_duration(&self) -> Option<f64> {
        let active_pipeline = self.pipeline();
        let dur: Option<gst::ClockTime> = {
            // Create a new duration query and send it to the pipeline.
            // This will traverse all elements in the pipeline, until one feels
            // capable of answering the query.
            let mut q = gst::query::Duration::new(gst::Format::Time);
            if active_pipeline.query(&mut q) {
                Some(q.result())
            } else {
                None
            }
        }
        .and_then(|dur| dur.try_into().ok())?;
        match dur {
            Some(d) => Some(d.seconds() as f64),
            None => None,
        }
    }

    fn query_duration(&self) -> glib::Continue {
        match self.pipeline_duration() {
            Some(duration) => {
                self.duration.replace(Some(duration));
                glib::Continue(false)
            }
            None => {
                self.duration.replace(None);
                glib::Continue(true)
            }
        }
    }

    pub fn duration(&self) -> Option<f64> {
        self.duration.borrow().clone()
    }

    //BUS SETUP
    fn connect_bus(self: Rc<Self>) {
        let bus = self.pipeline.bus().unwrap();
        bus.add_watch_local(
            clone!(@strong self as this => @default-return Continue(false), move |_, message| {
                let backend = this.clone();
                backend.handle_bus_message(message, 0)
            }),
        )
        .unwrap();
        //debug!("connect bus")
    }

    fn connect_bus_next(self: Rc<Self>) {
        let bus = self.pipeline_next.bus().unwrap();
        bus.add_watch_local(
            clone!(@strong self as this => @default-return Continue(false), move |_, message| {
                let backend = this.clone();
                backend.handle_bus_message(message, 1)
            }),
        )
        .unwrap();
        //debug!("connect bus next")
    }

    fn handle_bus_message(self: Rc<Self>, message: &gst::Message, pipeline_id: u8) -> Continue {
        use gst::MessageView;

        match message.view() {
            MessageView::Error(ref message) => self.on_bus_error(message),
            MessageView::Eos(_) => {
                // Only trigger EOS if it's from the active pipeline
                if pipeline_id == self.active_pipeline.get() {
                    self.on_bus_eos()
                }
            }
            MessageView::StateChanged(ref message) => self.on_gst_state_changed(message),
            MessageView::NewClock(ref message) => self.on_new_clock(message),
            // MessageView::Element(ref message) => self.on_bus_element(message),
            MessageView::StreamStart(_) => self.on_stream_start(),
            _ => (),
        }

        Continue(true)
    }

    fn on_stream_start(self: Rc<Self>) {
        //debug!("BACKEND on_stream_start");
        let timeout_duration = Duration::from_millis(1);
        let _source_id = glib::timeout_add_local(
            timeout_duration,
            clone!(@strong self as this => @default-return Continue(false) , move || {
                this.query_duration()
            }),
        );
    }

    //CLOCK STUFF
    fn on_new_clock(&self, message: &gst::message::NewClock) {
        //debug!("on new clock");
        self.clock_id.replace(None);
        self.clock.replace(message.clock());
        let clock = self.clock.borrow();
        match clock.as_ref() {
            Some(clock) => {
                let clock_id =
                    clock.new_periodic_id(clock.time().unwrap(), gst::ClockTime::from_seconds(1));

                self.clock_id.replace(Some(clock_id));

                //let player_sender = Rc::new(self.player_sender.borrow().as_ref().unwrap());
                self.clock_id
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .wait_async(
                        clone!(@strong self.sender as sender => move |_clock, time, _id| {
                            if let Some(time) = time {
                                let sec = time.seconds();
                                match sender.send(PlaybackAction::Tick(sec)) {
                                    Ok(()) => (),
                                    Err(err) => error!("{}", err)
                                }
                                //send!(sender, PlaybackAction::Tick(time.seconds()));
                            }
                        }),
                    )
                    .expect("Failed to wait async");
            }
            None => {
                return;
            }
        }
    }

    fn on_bus_error(&self, message: &gst::message::Error) {
        let error = message.error();
        let debug = message.debug();

        error!(
            "Error from element `{}`: {:?}",
            message.src().unwrap().name(),
            error
        );

        if let Some(debug) = debug {
            debug!("Debug info: {}", debug);
        }

        ////debug!("Error while playing audio with uri `{}`", self.imp().uri());

        self.set_state(BackendPlaybackState::Stopped);
        send!(self.sender, PlaybackAction::Error);
        //self.emit_by_name::<()>("error", &[]);
    }

    fn on_bus_eos(&self) {
        if self.is_crossfading.get() {
            // If we are crossfading, the 'next' action is already handled.
            // The EOS from the faded-out track should be ignored.
            debug!("EOS received during crossfade, ignoring.");
            return;
        }
        self.set_state(BackendPlaybackState::Stopped);
        send!(self.sender, PlaybackAction::EOS);
        //self.emit_by_name::<()>("eos", &[]);
    }

    fn on_gst_state_changed(&self, message: &gst::message::StateChanged) {
        let is_primary = message.src() == Some(self.pipeline.upcast_ref::<gst::Object>());
        let is_secondary = message.src() == Some(self.pipeline_next.upcast_ref::<gst::Object>());

        if !is_primary && !is_secondary {
            return;
        }

        let gst_state = message.current();

        debug!(
            "BACKEND gst state changed: `{:?}` -> `{:?}`",
            message.old(),
            gst_state
        );

        // Only update state for the active pipeline
        if (is_primary && self.active_pipeline.get() == 0)
            || (is_secondary && self.active_pipeline.get() == 1)
        {
            let backend_state = match gst_state {
                gst::State::Null => BackendPlaybackState::Stopped,
                gst::State::Ready => BackendPlaybackState::Loading,
                gst::State::Paused => BackendPlaybackState::Paused,
                gst::State::Playing => BackendPlaybackState::Playing,
                _ => return,
            };

            if self.state.get() != backend_state {
                self.state.set(backend_state);
                send!(self.sender, PlaybackAction::PlaybackState(backend_state));
            }

            //pipeline will change volume sometimes?
            if backend_state == BackendPlaybackState::Playing && self.volume() != self.volume.get()
            {
                //debug!("RESET VOLUME {:?}, {:?}", self.volume.get(), self.volume());
                self.set_volume_internal();
                //self.set_volume(self.volume.get())
            }
        }
    }

    pub fn seek(&self, seconds: u64) {
        //self._seek = self.pipeline.seek_simple(Gst.Format.TIME, Gst.SeekFlags.FLUSH | Gst.SeekFlags.KEY_UNIT, seconds * Gst.SECOND)

        match self.pipeline().seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            seconds * gst::ClockTime::SECOND,
        ) {
            Ok(_) => {
                debug!("seek success");
            }
            Err(_) => (),
        }
    }

    // CROSSFADE METHODS
    pub fn start_crossfade(self: Rc<Self>, next_uri: String) {
        if self.is_crossfading.get() {
            debug!("Already crossfading, ignoring start_crossfade");
            return;
        }

        debug!("Starting crossfade to next track");
        self.is_crossfading.set(true);

        let current_index = self.active_pipeline.get();
        let (current_pipeline_ref, next_pipeline_ref) = if current_index == 0 {
            (&self.pipeline, &self.pipeline_next)
        } else {
            (&self.pipeline_next, &self.pipeline)
        };

        let current_pipeline = current_pipeline_ref.clone();
        let next_pipeline = next_pipeline_ref.clone();

        // Prepare next track on the inactive pipeline
        let uri_encoded = urlencoding::encode(&next_uri);
        let replaced = uri_encoded.replace("%2F", "/");
        next_pipeline_ref.set_property("uri", format!("file:{}", replaced).to_value());

        // Set initial volume to 0 for fade-in
        self.set_volume_for_pipeline(next_pipeline_ref, 0.0);

        // Start playing the next track
        if let Err(e) = next_pipeline_ref.set_state(gst::State::Playing) {
            error!("Failed to start next pipeline: {}", e);
            self.is_crossfading.set(false);
            return;
        }

        // Switch active pipeline immediately so timers/queries reflect the new track
        let new_active = if current_index == 0 { 1 } else { 0 };
        self.active_pipeline.set(new_active);

        // Ensure state listeners know we are still playing with the new pipeline
        self.state.set(BackendPlaybackState::Playing);
        send!(self.sender, PlaybackAction::PlaybackState(BackendPlaybackState::Playing));

        // Start the crossfade animation
        self.perform_crossfade(current_pipeline, next_pipeline);
    }

    fn perform_crossfade(
        self: Rc<Self>,
        current_pipeline: gst::Pipeline,
        next_pipeline: gst::Pipeline,
    ) {
        let crossfade_duration = self.crossfade_duration.get();
        let target_volume = self.volume.get();

        if crossfade_duration <= f64::EPSILON {
            self.set_volume_for_pipeline(&next_pipeline, target_volume);
            self.set_volume_for_pipeline(&current_pipeline, 0.0);
            let _ = current_pipeline.set_state(gst::State::Null);
            self.is_crossfading.set(false);
            return;
        }

        let steps = (crossfade_duration * 20.0).clamp(20.0, 200.0);
        let step_duration_ms = (crossfade_duration * 1000.0 / steps).max(10.0);
        let step_duration = Duration::from_millis(step_duration_ms as u64);

        let mut step = 0.0;
        // Perform the crossfade gradually
        glib::source::timeout_add_local(
            step_duration,
            clone!(@strong self as this, @strong current_pipeline, @strong next_pipeline => @default-return Continue(false), move || {
                step += 1.0;
                let progress = (step / steps).clamp(0.0, 1.0);

                if progress >= 1.0 {
                    // Crossfade complete
                    this.set_volume_for_pipeline(&next_pipeline, target_volume);
                    this.set_volume_for_pipeline(&current_pipeline, 0.0);

                    // Stop and clean up the old pipeline
                    let _ = current_pipeline.set_state(gst::State::Null);

                    this.is_crossfading.set(false);

                    return Continue(false);
                }

                // Fade out current, fade in next
                let current_vol = target_volume * (1.0 - progress);
                let next_vol = target_volume * progress;

                this.set_volume_for_pipeline(&current_pipeline, current_vol);
                this.set_volume_for_pipeline(&next_pipeline, next_vol);

                Continue(true)
            }),
        );
    }

    pub fn should_start_crossfade(&self) -> bool {
        if self.is_crossfading.get() {
            return false;
        }

        if let Some(duration) = self.pipeline_duration() {
            if let Some(position) = self.pipeline_position() {
                let remaining = duration - position as f64;
                return remaining <= self.crossfade_duration.get() && remaining > 0.0;
            }
        }
        false
    }
}
