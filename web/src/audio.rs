use crate::utils::JsResult;
use fnv::FnvHashMap;
use generational_arena::Arena;
use ruffle_core::backend::audio::decoders::{AdpcmDecoder, Mp3Decoder};
use ruffle_core::backend::audio::swf::{self, AudioCompression};
use ruffle_core::backend::audio::{AudioBackend, AudioStreamHandle, SoundHandle};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::AudioContext;

pub struct WebAudioBackend {
    context: AudioContext,
    sounds: Arena<Sound>,
    stream_data: FnvHashMap<swf::CharacterId, StreamData>,
    id_to_sound: FnvHashMap<swf::CharacterId, SoundHandle>,
    left_samples: Vec<f32>,
    right_samples: Vec<f32>,
    frame_rate: f64,
}

thread_local! {
    static SOUND_INSTANCES: RefCell<Arena<SoundInstance>> = RefCell::new(Arena::new());
    static NUM_SOUNDS_LOADING: Cell<u32> = Cell::new(0);
}

#[derive(Clone)]
struct StreamData {
    format: swf::SoundFormat,
    audio_data: Vec<u8>,
    num_sample_frames: u32,
    samples_per_block: u32,
    adpcm_block_offsets: Vec<usize>,

    /// List of stream segments. Contains the frame they start on and the starting sample.
    /// Guaranteed to be in frame order.
    stream_segments: Vec<(u16, u32)>,

    /// The last frame we received a `StreamSoundBlock` from.
    last_clip_frame: u16,
}

type AudioBufferPtr = Rc<RefCell<web_sys::AudioBuffer>>;

// A sound can be either as a JS AudioBuffer and as a on--the-fly decoded stream using a ScriptProcessorNode.
#[allow(dead_code)]
enum SoundSource {
    // Pre-decoded audio buffer.
    AudioBuffer(AudioBufferPtr),

    // Decode the audio data on the fly from a byte stream.
    Decoder(Vec<u8>),
}

#[allow(dead_code)]
struct Sound {
    num_sample_frames: u32,
    format: swf::SoundFormat,
    source: SoundSource,
}

type Decoder = Box<dyn Iterator<Item = [i16; 2]>>;

/// An actively playing instance of a sound.
/// This sound can be either an event sound (`StartSound`) or
/// a stream sound (`SoundStreamBlock`).
struct SoundInstance {
    /// Handle to the sound clip.
    handle: Option<SoundHandle>,

    /// Format of the sound.
    format: swf::SoundFormat,

    /// On web, sounds can be played via different methods:
    /// either decoded on the fly with Decoder, or pre-decoded
    /// and played with and AudioBufferSourceNode.
    instance_type: SoundInstanceType,
}

#[allow(dead_code)]
enum SoundInstanceType {
    Decoder(Decoder),
    AudioBuffer(web_sys::AudioBufferSourceNode),
}

type Error = Box<dyn std::error::Error>;

impl WebAudioBackend {
    pub fn new() -> Result<Self, Error> {
        let context = AudioContext::new().map_err(|_| "Unable to create AudioContext")?;
        Ok(Self {
            context,
            sounds: Arena::new(),
            stream_data: FnvHashMap::default(),
            id_to_sound: FnvHashMap::default(),
            left_samples: vec![],
            right_samples: vec![],
            frame_rate: 1.0,
        })
    }

    pub fn set_frame_rate(&mut self, frame_rate: f64) {
        self.frame_rate = frame_rate
    }

    fn start_sound_internal(
        &mut self,
        handle: SoundHandle,
        settings: Option<&swf::SoundInfo>,
    ) -> SoundHandle {
        let sound = self.sounds.get(handle).unwrap();
        match &sound.source {
            SoundSource::AudioBuffer(audio_buffer) => {
                let audio_buffer = audio_buffer.borrow();
                let node = self.context.create_buffer_source().unwrap();
                node.set_buffer(Some(&*audio_buffer));
                node.connect_with_audio_node(&self.context.destination())
                    .warn_on_error();

                match settings {
                    Some(settings)
                        if settings.num_loops > 1
                            || settings.in_sample.is_some()
                            || settings.out_sample.is_some()
                            || settings.envelope.is_some() =>
                    {
                        // Event sound with non-default parameters.
                        // Note that start/end values are in 44.1kHZ samples regardless of the sound's sample rate.
                        let start_sample_frame =
                            f64::from(settings.in_sample.unwrap_or(0)) / 44100.0;
                        node.set_loop(settings.num_loops > 1);
                        node.set_loop_start(start_sample_frame);
                        node.start_with_when_and_grain_offset(0.0, start_sample_frame)
                            .warn_on_error();
                        // If we are looping or have a custom end point, we have to manually stop the sound.
                        if settings.out_sample.is_some() || settings.num_loops > 1 {
                            let end_sample_frame = if let Some(out_sample) = settings.out_sample {
                                f64::from(out_sample) / 44100.0
                            } else {
                                f64::from(sound.num_sample_frames)
                                    / f64::from(sound.format.sample_rate)
                            };
                            // `AudioSourceBufferNode.loop` is a bool, so we have to stop the loop at the proper time.
                            // `start_with_when_and_grain_offset_and_grain_duration` unfortunately doesn't work
                            // as you might expect with loops, so we use `stop_with_when` to stop the loop.
                            let total_len = (end_sample_frame - start_sample_frame)
                                * f64::from(settings.num_loops);
                            let current_time = self.context.current_time();
                            node.set_loop_end(end_sample_frame);
                            node.stop_with_when(current_time + total_len)
                                .warn_on_error();
                        }
                    }
                    _ => {
                        // Default event sound or stream.
                        node.start().warn_on_error();
                    }
                }

                let instance = SoundInstance {
                    handle: Some(handle),
                    format: sound.format.clone(),
                    instance_type: SoundInstanceType::AudioBuffer(node),
                };
                SOUND_INSTANCES.with(|instances| {
                    let mut instances = instances.borrow_mut();
                    instances.insert(instance)
                })
            }
            SoundSource::Decoder(audio_data) => {
                let decoder: Decoder = match sound.format.compression {
                    AudioCompression::Adpcm => Box::new(AdpcmDecoder::new(
                        std::io::Cursor::new(audio_data.to_vec()),
                        sound.format.is_stereo,
                        sound.format.sample_rate,
                    )),
                    AudioCompression::Mp3 => Box::new(Mp3Decoder::new(
                        if sound.format.is_stereo { 2 } else { 1 },
                        sound.format.sample_rate.into(),
                        std::io::Cursor::new(audio_data.to_vec()), //&sound.data[..]
                    )),
                    _ => unimplemented!(),
                };

                let decoder: Decoder =
                    if sound.format.sample_rate != self.context.sample_rate() as u16 {
                        Box::new(resample(
                            decoder,
                            sound.format.sample_rate,
                            self.context.sample_rate() as u16,
                        ))
                    } else {
                        decoder
                    };

                let instance = SoundInstance {
                    handle: Some(handle),
                    format: sound.format.clone(),
                    instance_type: SoundInstanceType::Decoder(decoder),
                };
                SOUND_INSTANCES.with(|instances| {
                    let mut instances = instances.borrow_mut();
                    let instance_handle = instances.insert(instance);
                    let script_processor_node = self.context.create_script_processor_with_buffer_size_and_number_of_input_channels_and_number_of_output_channels(4096, 0, if sound.format.is_stereo { 2 } else { 1 }).unwrap();
                    let script_node = script_processor_node.clone();
                    let closure = Closure::wrap(Box::new(move |event| {
                            SOUND_INSTANCES.with(|instances| {
                                let mut instances = instances.borrow_mut();
                                let instance = instances.get_mut(instance_handle).unwrap();
                                let complete = WebAudioBackend::update_script_processor(instance, event);
                                if complete {
                                    instances.remove(instance_handle);
                                    script_node.disconnect().unwrap();
                                }
                            })
                        }) as Box<dyn FnMut(web_sys::AudioProcessingEvent)>);
                        script_processor_node.set_onaudioprocess(Some(closure.as_ref().unchecked_ref()));
                        // TODO: This will leak memory per playing sound. Remember and properly drop the closure.
                        closure.forget();

                    instance_handle
                })
            }
        }
    }

    fn decompress_to_audio_buffer(
        &mut self,
        format: &swf::SoundFormat,
        audio_data: &[u8],
        num_sample_frames: u32,
        adpcm_block_offsets: Option<&[usize]>,
    ) -> AudioBufferPtr {
        if format.compression == AudioCompression::Mp3 {
            return self.decompress_mp3_to_audio_buffer(format, audio_data, num_sample_frames);
        }

        match format.compression {
            AudioCompression::Uncompressed => {
                // TODO: Check for is_16_bit.
                self.left_samples = audio_data
                    .iter()
                    .step_by(2)
                    .cloned()
                    .map(|n| f32::from(n) / 32767.0)
                    .collect();
                if format.is_stereo {
                    self.right_samples = audio_data
                        .iter()
                        .skip(1)
                        .step_by(2)
                        .cloned()
                        .map(|n| f32::from(n) / 32767.0)
                        .collect();
                }
            }
            AudioCompression::Adpcm => {
                // For stream sounds, the ADPCM header is included in each block,
                // so we must recreate the decoder for each block.
                // Event sounds don't have this issue.
                let full = [0, audio_data.len()];
                let adpcm_block_offsets = adpcm_block_offsets.unwrap_or(&full);
                self.left_samples.clear();
                self.right_samples.clear();
                for block in adpcm_block_offsets.windows(2) {
                    let start = block[0];
                    let end = block[1];
                    let mut decoder = AdpcmDecoder::new(
                        &audio_data[start..end],
                        format.is_stereo,
                        format.sample_rate,
                    );
                    if format.is_stereo {
                        while let Some(frame) = decoder.next() {
                            let (l, r) = (frame[0], frame[1]);
                            self.left_samples.push(f32::from(l) / 32767.0);
                            self.right_samples.push(f32::from(r) / 32767.0);
                        }
                    } else {
                        self.left_samples
                            .extend(decoder.map(|n| f32::from(n[0]) / 32767.0));
                    }
                }
            }
            _ => unimplemented!(),
        }

        // Double up samples for 5512Hz audio to satisfy Firefox.
        if format.sample_rate == 5512 {
            let mut samples = Vec::with_capacity(self.left_samples.len() * 2);
            for sample in &self.left_samples {
                samples.push(*sample);
                samples.push(*sample);
            }
            self.left_samples = samples;

            if format.is_stereo {
                let mut samples = Vec::with_capacity(self.right_samples.len() * 2);
                for sample in &self.right_samples {
                    samples.push(*sample);
                    samples.push(*sample);
                }
                self.right_samples = samples;
            }
        }

        // This sucks. Firefox doesn't like 5512Hz sample rate, so manually double up the samples.
        // 5512Hz should be relatively rare.
        let num_sample_frames = self.left_samples.len() as u32;
        let audio_buffer = if format.sample_rate > 5512 {
            self.context
                .create_buffer(
                    if format.is_stereo { 2 } else { 1 },
                    num_sample_frames,
                    f32::from(format.sample_rate),
                )
                .unwrap()
        } else {
            self.context
                .create_buffer(
                    if format.is_stereo { 2 } else { 1 },
                    num_sample_frames * 2,
                    11025.0,
                )
                .unwrap()
        };

        audio_buffer
            .copy_to_channel(&mut self.left_samples, 0)
            .unwrap();
        if format.is_stereo {
            audio_buffer
                .copy_to_channel(&mut self.right_samples, 1)
                .unwrap();
        }

        Rc::new(RefCell::new(audio_buffer))
    }

    fn decompress_mp3_to_audio_buffer(
        &mut self,
        format: &swf::SoundFormat,
        audio_data: &[u8],
        _num_sample_frames: u32,
    ) -> AudioBufferPtr {
        // We use the Web decodeAudioData API to decode MP3 data.
        // TODO: Is it possible we finish loading before the MP3 is decoding?
        let audio_buffer = self
            .context
            .create_buffer(1, 1, self.context.sample_rate())
            .unwrap();
        let audio_buffer = Rc::new(RefCell::new(audio_buffer));

        let data_array = unsafe { js_sys::Uint8Array::view(&audio_data[..]) };
        let array_buffer = data_array.buffer().slice_with_end(
            data_array.byte_offset(),
            data_array.byte_offset() + data_array.byte_length(),
        );

        NUM_SOUNDS_LOADING.with(|n| n.set(n.get() + 1));

        let _num_channels = if format.is_stereo { 2 } else { 1 };
        let buffer_ptr = Rc::clone(&audio_buffer);
        let success_closure = Closure::wrap(Box::new(move |buffer: web_sys::AudioBuffer| {
            *buffer_ptr.borrow_mut() = buffer;
            NUM_SOUNDS_LOADING.with(|n| n.set(n.get() - 1));
        }) as Box<dyn FnMut(web_sys::AudioBuffer)>);
        let error_closure = Closure::wrap(Box::new(move || {
            log::info!("Error decoding MP3 audio");
            NUM_SOUNDS_LOADING.with(|n| n.set(n.get() - 1));
        }) as Box<dyn FnMut()>);
        self.context
            .decode_audio_data_with_success_callback_and_error_callback(
                &array_buffer,
                success_closure.as_ref().unchecked_ref(),
                error_closure.as_ref().unchecked_ref(),
            )
            .unwrap();

        // TODO: This will leak memory (once per decompressed MP3).
        // Not a huge deal as there are probably not many MP3s in an SWF.
        success_closure.forget();
        error_closure.forget();

        audio_buffer
    }

    fn update_script_processor(
        instance: &mut SoundInstance,
        event: web_sys::AudioProcessingEvent,
    ) -> bool {
        let mut complete = false;
        let mut left_samples = vec![];
        let mut right_samples = vec![];
        if let SoundInstanceType::Decoder(ref mut decoder) = &mut instance.instance_type {
            let output_buffer = event.output_buffer().unwrap();
            let num_frames = output_buffer.length() as usize;

            for _ in 0..num_frames {
                if let Some(frame) = decoder.next() {
                    let (l, r) = (frame[0], frame[1]);
                    left_samples.push(f32::from(l) / 32767.0);
                    if instance.format.is_stereo {
                        right_samples.push(f32::from(r) / 32767.0);
                    }
                } else {
                    complete = true;
                    break;
                }
            }
            output_buffer
                .copy_to_channel(&mut left_samples[..], 0)
                .unwrap();
            if instance.format.is_stereo {
                output_buffer
                    .copy_to_channel(&mut right_samples[..], 1)
                    .unwrap();
            }
        }

        complete
    }
}

impl AudioBackend for WebAudioBackend {
    fn register_sound(&mut self, sound: &swf::Sound) -> Result<SoundHandle, Error> {
        // Slice off latency seek for MP3 data.
        let data = if sound.format.compression == AudioCompression::Mp3 {
            &sound.data[2..]
        } else {
            &sound.data[..]
        };

        let sound = Sound {
            num_sample_frames: sound.num_samples,
            format: sound.format.clone(),
            source: SoundSource::AudioBuffer(self.decompress_to_audio_buffer(
                &sound.format,
                data,
                sound.num_samples,
                None,
            )),
        };
        Ok(self.sounds.insert(sound))
    }

    fn preload_sound_stream_head(
        &mut self,
        clip_id: swf::CharacterId,
        _stream_start_frame: u16,
        stream_info: &swf::SoundStreamHead,
    ) {
        self.stream_data
            .entry(clip_id)
            .or_insert_with(|| StreamData {
                format: stream_info.stream_format.clone(),
                audio_data: vec![],
                num_sample_frames: 0,
                samples_per_block: stream_info.num_samples_per_block.into(),
                adpcm_block_offsets: vec![],
                stream_segments: vec![],
                last_clip_frame: 0,
            });
    }

    fn preload_sound_stream_block(
        &mut self,
        clip_id: swf::CharacterId,
        clip_frame: u16,
        audio_data: &[u8],
    ) {
        if let Some(stream) = self.stream_data.get_mut(&clip_id) {
            // Handle gaps in streaming audio. Store the offsets for each stream segment.
            if stream.audio_data.is_empty() || stream.last_clip_frame + 1 != clip_frame {
                let sample_mult = 44100 / stream.format.sample_rate;
                let start_sample = stream.num_sample_frames * u32::from(sample_mult);
                stream.stream_segments.push((clip_frame, start_sample));
            }
            stream.last_clip_frame = clip_frame;

            match stream.format.compression {
                AudioCompression::Uncompressed | AudioCompression::UncompressedUnknownEndian => {
                    let frame_len = if stream.format.is_stereo { 2 } else { 1 }
                        * if stream.format.is_16_bit { 2 } else { 1 };
                    stream.num_sample_frames += (audio_data.len() as u32) / frame_len;
                    stream.audio_data.extend_from_slice(audio_data);
                }
                AudioCompression::Mp3 => {
                    let num_sample_frames =
                        u32::from(audio_data[0]) | (u32::from(audio_data[1]) << 8);
                    stream.num_sample_frames += num_sample_frames;
                    // MP3 streaming data:
                    // First two bytes = number of samples
                    // Second two bytes = 'latency seek' (amount to skip when seeking to this frame)
                    stream.audio_data.extend_from_slice(&audio_data[4..]);
                }
                AudioCompression::Adpcm => {
                    // For ADPCM data, we must keep track of where each block starts,
                    // so that we read the header in each block.
                    stream.num_sample_frames += stream.samples_per_block;
                    stream.adpcm_block_offsets.push(stream.audio_data.len());
                    stream.audio_data.extend_from_slice(audio_data);
                }
                _ => {
                    // TODO: This is a guess and will vary slightly from block to block!
                    stream.num_sample_frames += stream.samples_per_block;
                }
            }
        }
    }

    fn preload_sound_stream_end(&mut self, clip_id: swf::CharacterId) {
        let stream_data = self.stream_data.remove(&clip_id);

        if let Some(mut stream) = stream_data {
            // Only worry about streams that actually have data.
            if !stream.audio_data.is_empty() {
                let audio_buffer = self.decompress_to_audio_buffer(
                    &stream.format,
                    &stream.audio_data[..],
                    stream.num_sample_frames,
                    if stream.format.compression == AudioCompression::Adpcm {
                        stream.adpcm_block_offsets.push(stream.audio_data.len());
                        Some(&stream.adpcm_block_offsets[..])
                    } else {
                        None
                    },
                );
                stream.audio_data = vec![];
                self.stream_data.insert(clip_id, stream.clone());
                let handle = self.sounds.insert(Sound {
                    format: stream.format,
                    num_sample_frames: stream.num_sample_frames,
                    source: SoundSource::AudioBuffer(audio_buffer),
                });
                self.id_to_sound.insert(clip_id, handle);
            }
        }
    }

    fn start_sound(&mut self, sound: SoundHandle, sound_info: &swf::SoundInfo) {
        self.start_sound_internal(sound, Some(sound_info));
    }

    fn start_stream(
        &mut self,
        clip_id: swf::CharacterId,
        clip_frame: u16,
        _clip_data: ruffle_core::tag_utils::SwfSlice,
        _stream_info: &swf::SoundStreamHead,
    ) -> AudioStreamHandle {
        if let Some(&handle) = self.id_to_sound.get(&clip_id) {
            let mut sound_info = None;
            if clip_frame > 1 {
                if let Some(stream_data) = self.stream_data.get(&clip_id) {
                    // Figure out the frame and sample where this stream segment first starts.
                    let start_pos = match stream_data
                        .stream_segments
                        .binary_search_by(|(f, _)| f.cmp(&clip_frame))
                    {
                        Ok(i) => stream_data.stream_segments[i].1,
                        Err(i) => {
                            if i > 0 {
                                let (segment_frame, segment_sample) =
                                    stream_data.stream_segments[i - 1];
                                let frames_skipped = clip_frame.saturating_sub(segment_frame);
                                let samples_per_frame = 44100.0 / self.frame_rate;
                                segment_sample
                                    + u32::from(frames_skipped) * (samples_per_frame as u32)
                            } else {
                                0
                            }
                        }
                    };
                    sound_info = Some(swf::SoundInfo {
                        event: swf::SoundEvent::Event,
                        in_sample: Some(start_pos),
                        out_sample: None,
                        num_loops: 1,
                        envelope: None,
                    });
                }
            }
            self.start_sound_internal(handle, sound_info.as_ref())
        } else {
            log::error!("Missing stream for clip {}", clip_id);
            // TODO: Return dummy sound.
            panic!();
        }
    }

    fn stop_stream(&mut self, stream: AudioStreamHandle) {
        SOUND_INSTANCES.with(|instances| {
            let mut instances = instances.borrow_mut();
            if let Some(mut instance) = instances.remove(stream) {
                if let SoundInstanceType::AudioBuffer(ref mut node) = instance.instance_type {
                    let _ = node.stop();
                }
            }
        })
    }

    fn is_loading_complete(&self) -> bool {
        NUM_SOUNDS_LOADING.with(|n| n.get() == 0)
    }

    fn prime_audio(&mut self) {
        // Allow audio to start playing after a user gesture.
        let _ = self.context.resume();
    }

    fn stop_all_sounds(&mut self) {
        SOUND_INSTANCES.with(|instances| {
            let mut instances = instances.borrow_mut();
            instances.iter_mut().for_each(|(_, instance)| {
                if let SoundInstanceType::AudioBuffer(ref mut node) = instance.instance_type {
                    let _ = node.stop();
                }
                // TODO: Have to handle Decoder nodes. (These may just go into a different backend.)
            });
            instances.clear();
        })
    }

    fn stop_sounds_with_handle(&mut self, handle: SoundHandle) {
        SOUND_INSTANCES.with(|instances| {
            let mut instances = instances.borrow_mut();
            let handle = Some(handle);
            instances.retain(|_, instance| instance.handle != handle);
        })
    }

    fn is_sound_playing_with_handle(&mut self, handle: SoundHandle) -> bool {
        SOUND_INSTANCES.with(|instances| {
            let instances = instances.borrow();
            let handle = Some(handle);
            instances
                .iter()
                .any(|(_, instance)| instance.handle == handle)
        })
    }
}

// Janky resmapling code.
// TODO: Clean this up.
fn resample(
    mut input: impl Iterator<Item = [i16; 2]>,
    input_sample_rate: u16,
    output_sample_rate: u16,
) -> impl Iterator<Item = [i16; 2]> {
    let (mut left0, mut right0) = if let Some(frame) = input.next() {
        (Some(frame[0]), Some(frame[1]))
    } else {
        (None, None)
    };
    let (mut left1, mut right1) = if let Some(frame) = input.next() {
        (Some(frame[0]), Some(frame[1]))
    } else {
        (None, None)
    };
    let (mut left, mut right) = (left0.unwrap(), right0.unwrap());
    let dt_input = 1.0 / f64::from(input_sample_rate);
    let dt_output = 1.0 / f64::from(output_sample_rate);
    let mut t = 0.0;
    std::iter::from_fn(move || {
        if let (Some(l0), Some(r0), Some(l1), Some(r1)) = (left0, right0, left1, right1) {
            let a = t / dt_input;
            let l0 = f64::from(l0);
            let l1 = f64::from(l1);
            let r0 = f64::from(r0);
            let r1 = f64::from(r1);
            left = (l0 + (l1 - l0) * a) as i16;
            right = (r0 + (r1 - r0) * a) as i16;
            t += dt_output;
            while t >= dt_input {
                t -= dt_input;
                left0 = left1;
                right0 = right1;
                if let Some(frame) = input.next() {
                    left1 = Some(frame[0]);
                    right1 = Some(frame[1]);
                } else {
                    left1 = None;
                    right1 = None;
                }
            }
            Some([left, right])
        } else {
            None
        }
    })
}
