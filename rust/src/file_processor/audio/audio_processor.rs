#[cfg(feature = "mkl")]
extern crate intel_mkl_src;

#[cfg(feature = "accelerate")]
extern crate accelerate_src;

use std::path::PathBuf;

use anyhow::{Error as E, Result};
use candle_core::{Device, IndexOp, Tensor};
use candle_nn::{ops::softmax, VarBuilder};
use hf_hub::{api::sync::Api, Repo, RepoType};
use rand::{
    distr::{weighted::WeightedIndex, Distribution},
    SeedableRng,
};
use tokenizers::Tokenizer;

use candle_transformers::models::whisper::{self as m, Config};

use crate::embeddings::select_device;

#[cfg(feature = "audio")]
use {crate::embeddings::embed::AudioDecoder, candle_transformers::models::whisper::audio};

pub enum WhichAudioDecoderModel {
    Normal(m::model::Whisper),
    Quantized(m::quantized_model::Whisper),
}

impl WhichAudioDecoderModel {
    pub fn config(&self) -> &Config {
        match self {
            Self::Normal(m) => &m.config,
            Self::Quantized(m) => &m.config,
        }
    }

    pub fn encoder_forward(&mut self, x: &Tensor, flush: bool) -> candle_core::Result<Tensor> {
        match self {
            Self::Normal(m) => m.encoder.forward(x, flush),
            Self::Quantized(m) => m.encoder.forward(x, flush),
        }
    }

    pub fn decoder_forward(
        &mut self,
        x: &Tensor,
        xa: &Tensor,
        flush: bool,
    ) -> candle_core::Result<Tensor> {
        match self {
            Self::Normal(m) => m.decoder.forward(x, xa, flush),
            Self::Quantized(m) => m.decoder.forward(x, xa, flush),
        }
    }

    pub fn decoder_final_linear(&self, x: &Tensor) -> candle_core::Result<Tensor> {
        match self {
            Self::Normal(m) => m.decoder.final_linear(x),
            Self::Quantized(m) => m.decoder.final_linear(x),
        }
    }
}

pub struct AudioDecoderModel {
    pub model: WhichAudioDecoderModel,
    pub tokenizer: Tokenizer,
    pub config: Config,
    pub device: Device,
}

#[derive(Debug, Clone, Default)]
pub struct DecodingResult {
    pub tokens: Vec<u32>,
    pub text: String,
    pub avg_logprob: f64,
    pub no_speech_prob: f64,
    pub temperature: f64,
    pub compression_ratio: f64,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Segment {
    pub start: f64,
    pub duration: f64,
    pub dr: DecodingResult,
}

#[allow(dead_code)]
pub enum Task {
    Transcribe,
    Translate,
}
pub struct Decoder<'a> {
    pub model: &'a mut AudioDecoderModel,
    pub rng: rand::rngs::StdRng,
    pub task: Option<Task>,
    pub timestamps: bool,
    pub verbose: bool,
    pub suppress_tokens: Tensor,
    pub sot_token: u32,
    pub transcribe_token: u32,
    pub translate_token: u32,
    pub eot_token: u32,
    pub no_speech_token: u32,
    pub no_timestamps_token: u32,
    pub language_token: Option<u32>,
}

impl<'a> Decoder<'a> {
    pub fn new(
        model: &'a mut AudioDecoderModel,
        seed: u64,
        device: &Device,
        language_token: Option<u32>,
        task: Option<Task>,
        timestamps: bool,
        verbose: bool,
    ) -> Result<Self> {
        let no_timestamps_token = token_id(&model.tokenizer, m::NO_TIMESTAMPS_TOKEN)?;
        // Suppress the notimestamps token when in timestamps mode.
        let suppress_tokens: Vec<f32> = (0..model.model.config().vocab_size as u32)
            .map(|i| {
                if model.model.config().suppress_tokens.contains(&i)
                    || timestamps && i == no_timestamps_token
                {
                    f32::NEG_INFINITY
                } else {
                    0f32
                }
            })
            .collect();
        let suppress_tokens = Tensor::new(suppress_tokens.as_slice(), device)?;
        let sot_token = token_id(&model.tokenizer, m::SOT_TOKEN)?;
        let transcribe_token = token_id(&model.tokenizer, m::TRANSCRIBE_TOKEN)?;
        let translate_token = token_id(&model.tokenizer, m::TRANSLATE_TOKEN)?;
        let eot_token = token_id(&model.tokenizer, m::EOT_TOKEN)?;
        let no_speech_token = m::NO_SPEECH_TOKENS
            .iter()
            .find_map(|token| token_id(&model.tokenizer, token).ok());
        let no_speech_token = match no_speech_token {
            None => anyhow::bail!("unable to find any non-speech token"),
            Some(n) => n,
        };
        Ok(Self {
            model,
            rng: rand::rngs::StdRng::seed_from_u64(seed),
            task,
            timestamps,
            verbose,
            suppress_tokens,
            sot_token,
            transcribe_token,
            translate_token,
            eot_token,
            no_speech_token,
            language_token,
            no_timestamps_token,
        })
    }

    pub fn decode(&mut self, mel: &Tensor, t: f64) -> Result<DecodingResult> {
        let model = &mut self.model;
        let audio_features = model.model.encoder_forward(mel, true)?;
        if self.verbose {
            println!("audio features: {:?}", audio_features.dims());
        }
        let sample_len = model.model.config().max_target_positions / 2;
        let mut sum_logprob = 0f64;
        let mut no_speech_prob = f64::NAN;
        let mut tokens = vec![self.sot_token];
        if let Some(language_token) = self.language_token {
            tokens.push(language_token);
        }
        match self.task {
            None | Some(Task::Transcribe) => tokens.push(self.transcribe_token),
            Some(Task::Translate) => tokens.push(self.translate_token),
        }
        if !self.timestamps {
            tokens.push(self.no_timestamps_token);
        }
        for i in 0..sample_len {
            let tokens_t = Tensor::new(tokens.as_slice(), mel.device())?;

            // The model expects a batch dim but this inference loop does not handle
            // it so we add it at this point.
            let tokens_t = tokens_t.unsqueeze(0)?;
            let ys = model
                .model
                .decoder_forward(&tokens_t, &audio_features, i == 0)?;

            // Extract the no speech probability on the first iteration by looking at the first
            // token logits and the probability for the according token.
            if i == 0 {
                let logits = model.model.decoder_final_linear(&ys.i(..1)?)?.i(0)?.i(0)?;
                no_speech_prob = softmax(&logits, 0)?
                    .i(self.no_speech_token as usize)?
                    .to_scalar::<f32>()? as f64;
            }

            let (_, seq_len, _) = ys.dims3()?;
            let logits = model
                .model
                .decoder_final_linear(&ys.i((..1, seq_len - 1..))?)?
                .i(0)?
                .i(0)?;
            // TODO: Besides suppress tokens, we should apply the heuristics from
            // ApplyTimestampRules, i.e.:
            // - Timestamps come in pairs, except before EOT.
            // - Timestamps should be non-decreasing.
            // - If the sum of the probabilities of timestamps is higher than any other tokens,
            //   only consider timestamps when sampling.
            // https://github.com/openai/whisper/blob/e8622f9afc4eba139bf796c210f5c01081000472/whisper/decoding.py#L439
            let logits = logits.broadcast_add(&self.suppress_tokens)?;
            let next_token = if t > 0f64 {
                let prs = softmax(&(&logits / t)?, 0)?;
                let logits_v: Vec<f32> = prs.to_vec1()?;
                let distr = WeightedIndex::new(&logits_v)?;
                distr.sample(&mut self.rng) as u32
            } else {
                let logits_v: Vec<f32> = logits.to_vec1()?;
                logits_v
                    .iter()
                    .enumerate()
                    .max_by(|(_, u), (_, v)| u.total_cmp(v))
                    .map(|(i, _)| i as u32)
                    .unwrap()
            };
            tokens.push(next_token);
            let prob = softmax(&logits, candle_core::D::Minus1)?
                .i(next_token as usize)?
                .to_scalar::<f32>()? as f64;
            if next_token == self.eot_token
                || tokens.len() > model.model.config().max_target_positions
            {
                break;
            }
            sum_logprob += prob.ln();
        }
        let text = self.model.tokenizer.decode(&tokens, true).map_err(E::msg)?;
        let avg_logprob = sum_logprob / tokens.len() as f64;

        Ok(DecodingResult {
            tokens,
            text,
            avg_logprob,
            no_speech_prob,
            temperature: t,
            compression_ratio: f64::NAN,
        })
    }

    pub fn decode_with_fallback(&mut self, segment: &Tensor) -> Result<DecodingResult> {
        for (i, &t) in m::TEMPERATURES.iter().enumerate() {
            let dr: Result<DecodingResult> = self.decode(segment, t);
            if i == m::TEMPERATURES.len() - 1 {
                return dr;
            }
            // On errors, we try again with a different temperature.
            match dr {
                Ok(dr) => {
                    let needs_fallback = dr.compression_ratio > m::COMPRESSION_RATIO_THRESHOLD
                        || dr.avg_logprob < m::LOGPROB_THRESHOLD;
                    if !needs_fallback || dr.no_speech_prob > m::NO_SPEECH_THRESHOLD {
                        return Ok(dr);
                    }
                }
                Err(err) => {
                    println!("Error running at {t}: {err}")
                }
            }
        }
        unreachable!()
    }

    pub fn run(&mut self, mel: &Tensor) -> Result<Vec<Segment>> {
        let (_, _, content_frames) = mel.dims3()?;
        let mut seek = 0;
        let mut segments = vec![];
        while seek < content_frames {
            let start = std::time::Instant::now();
            let time_offset = (seek * m::HOP_LENGTH) as f64 / m::SAMPLE_RATE as f64;
            let segment_size = usize::min(content_frames - seek, m::N_FRAMES);
            let mel_segment = mel.narrow(2, seek, segment_size)?;
            let segment_duration = (segment_size * m::HOP_LENGTH) as f64 / m::SAMPLE_RATE as f64;
            let dr = self.decode_with_fallback(&mel_segment)?;
            seek += segment_size;
            if dr.no_speech_prob > m::NO_SPEECH_THRESHOLD && dr.avg_logprob < m::LOGPROB_THRESHOLD {
                println!("no speech detected, skipping {seek} {dr:?}");
                continue;
            }
            let segment = Segment {
                start: time_offset,
                duration: segment_duration,
                dr,
            };
            if self.timestamps {
                println!(
                    "{:.1}s -- {:.1}s",
                    segment.start,
                    segment.start + segment.duration,
                );
                let mut tokens_to_decode = vec![];
                let mut prev_timestamp_s = 0f32;
                for &token in segment.dr.tokens.iter() {
                    if token == self.sot_token || token == self.eot_token {
                        continue;
                    }
                    // The no_timestamp_token is the last before the timestamp ones.
                    if token > self.no_timestamps_token {
                        let timestamp_s = (token - self.no_timestamps_token + 1) as f32 / 50.;
                        if !tokens_to_decode.is_empty() {
                            let text = self
                                .model
                                .tokenizer
                                .decode(&tokens_to_decode, true)
                                .map_err(E::msg)?;
                            println!("  {:.1}s-{:.1}s: {}", prev_timestamp_s, timestamp_s, text);
                            tokens_to_decode.clear()
                        }
                        prev_timestamp_s = timestamp_s;
                    } else {
                        tokens_to_decode.push(token)
                    }
                }
                if !tokens_to_decode.is_empty() {
                    let text = self
                        .model
                        .tokenizer
                        .decode(&tokens_to_decode, true)
                        .map_err(E::msg)?;
                    if !text.is_empty() {
                        println!("  {:.1}s-...: {}", prev_timestamp_s, text);
                    }
                    tokens_to_decode.clear()
                }
            } else {
                println!(
                    "{:.1}s -- {:.1}s: {}",
                    segment.start,
                    segment.start + segment.duration,
                    segment.dr.text,
                )
            }
            if self.verbose {
                println!("{seek}: {segment:?}, in {:?}", start.elapsed());
            }
            segments.push(segment)
        }
        Ok(segments)
    }
}

pub fn token_id(tokenizer: &Tokenizer, token: &str) -> candle_core::Result<u32> {
    match tokenizer.token_to_id(token) {
        None => candle_core::bail!("no token-id for {token}"),
        Some(id) => Ok(id),
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhichModel {
    Tiny,
    TinyEn,
    Base,
    BaseEn,
    Small,
    SmallEn,
    Medium,
    MediumEn,
    Large,
    LargeV2,
    LargeV3,
    DistilMediumEn,
    DistilLargeV2,
    DistilLargeV3,
}

impl From<&str> for WhichModel {
    fn from(s: &str) -> Self {
        match s {
            "tiny" => Self::Tiny,
            "tiny-en" => Self::TinyEn,
            "base" => Self::Base,
            "base-en" => Self::BaseEn,
            "small" => Self::Small,
            "small-en" => Self::SmallEn,
            "medium" => Self::Medium,
            "medium-en" => Self::MediumEn,
            "large" => Self::Large,
            "large-v2" => Self::LargeV2,
            "large-v3" => Self::LargeV3,
            "distil-medium-en" => Self::DistilMediumEn,
            "distil-large-v2" => Self::DistilLargeV2,
            "distil-large-v3" => Self::DistilLargeV3,
            _ => unimplemented!("no support for {s}"),
        }
    }
}

impl WhichModel {
    pub fn is_multilingual(&self) -> bool {
        match self {
            Self::Tiny
            | Self::Base
            | Self::Small
            | Self::Medium
            | Self::Large
            | Self::LargeV2
            | Self::LargeV3
            | Self::DistilLargeV2
            | Self::DistilLargeV3 => true,
            Self::TinyEn | Self::BaseEn | Self::SmallEn | Self::MediumEn | Self::DistilMediumEn => {
                false
            }
        }
    }

    fn model_and_revision(&self) -> (&'static str, &'static str) {
        match self {
            Self::Tiny => ("openai/whisper-tiny", "main"),
            Self::TinyEn => ("openai/whisper-tiny.en", "refs/pr/15"),
            Self::Base => ("openai/whisper-base", "refs/pr/22"),
            Self::BaseEn => ("openai/whisper-base.en", "refs/pr/13"),
            Self::Small => ("openai/whisper-small", "main"),
            Self::SmallEn => ("openai/whisper-small.en", "refs/pr/10"),
            Self::Medium => ("openai/whisper-medium", "main"),
            Self::MediumEn => ("openai/whisper-medium.en", "main"),
            Self::Large => ("openai/whisper-large", "refs/pr/36"),
            Self::LargeV2 => ("openai/whisper-large-v2", "refs/pr/57"),
            Self::LargeV3 => ("openai/whisper-large-v3", "main"),
            Self::DistilMediumEn => ("distil-whisper/distil-medium.en", "main"),
            Self::DistilLargeV2 => ("distil-whisper/distil-large-v2", "main"),
            Self::DistilLargeV3 => ("distil-whisper/distil-large-v3", "main"),
        }
    }
}

pub fn build_model(
    model_id: Option<&str>,
    revision: Option<&str>,
    quantized: bool,
    model_type: &str,
) -> Result<ModelInput> {
    let model_type = WhichModel::from(model_type);

    let (default_model, default_revision) = if quantized {
        ("lmz/candle-whisper", "main")
    } else {
        model_type.model_and_revision()
    };

    let (model_id, revision) = match (model_id, revision) {
        (Some(model_id), Some(revision)) => (model_id, revision),
        (Some(model_id), None) => (model_id, "main"),
        (None, Some(revision)) => (default_model, revision),
        (None, None) => (default_model, default_revision),
    };

    let api = Api::new()?;
    let repo = api.repo(Repo::with_revision(
        model_id.to_string(),
        RepoType::Model,
        revision.to_string(),
    ));

    let (config, tokenizer, model) = if quantized {
        let ext = match model_type {
            WhichModel::TinyEn => "tiny-en",
            WhichModel::Tiny => "tiny",
            _ => unimplemented!("no quantized support for {:?}", model_type),
        };

        let config = repo.get(&format!("config-{ext}.json"))?;
        let tokenizer = repo.get(&format!("tokenizer-{ext}.json"))?;
        let model = repo.get(&format!("model-{ext}-q80.gguf"))?;

        (config, tokenizer, model)
    } else {
        let config = repo.get("config.json")?;
        let tokenizer = repo.get("tokenizer.json")?;
        let model = repo.get("model.safetensors")?;

        (config, tokenizer, model)
    };

    Ok(ModelInput {
        config,
        tokenizer,
        model,
    })
}

pub struct ModelInput {
    pub config: PathBuf,
    pub tokenizer: PathBuf,
    pub model: PathBuf,
}

impl AudioDecoderModel {
    pub fn from_pretrained(
        model_id: Option<&str>,
        revision: Option<&str>,
        model_type: &str,
        quantized: bool,
    ) -> Result<Self> {
        let device = select_device();

        match quantized {
            false => {
                let model_input = build_model(model_id, revision, quantized, model_type)?;
                let (config_filename, tokenizer_filename, weights_filename) =
                    (model_input.config, model_input.tokenizer, model_input.model);

                let config: Config =
                    serde_json::from_str(&std::fs::read_to_string(config_filename)?)?;
                let tokenizer = Tokenizer::from_file(tokenizer_filename).map_err(E::msg)?;

                let vb = unsafe {
                    VarBuilder::from_mmaped_safetensors(&[weights_filename], m::DTYPE, &device)?
                };

                let model =
                    WhichAudioDecoderModel::Normal(m::model::Whisper::load(&vb, config.clone())?);

                Ok(Self {
                    model,
                    tokenizer,
                    config,
                    device,
                })
            }
            true => {
                let model_input = build_model(model_id, revision, quantized, model_type)?;
                let (config_filename, tokenizer_filename, weights_filename) =
                    (model_input.config, model_input.tokenizer, model_input.model);

                let config = std::fs::read_to_string(config_filename)?;
                let tokenizer = std::fs::read_to_string(tokenizer_filename)?;

                let config: Config = serde_json::from_str(&config)?;
                let tokenizer = Tokenizer::from_file(tokenizer).map_err(E::msg)?;

                let vb = candle_transformers::quantized_var_builder::VarBuilder::from_gguf(
                    weights_filename,
                    &device,
                )?;
                let model = WhichAudioDecoderModel::Quantized(m::quantized_model::Whisper::load(
                    &vb,
                    config.clone(),
                )?);

                Ok(Self {
                    model,
                    tokenizer,
                    config,
                    device,
                })
            }
        }
    }
}

#[cfg(feature = "audio")]
mod audio_processing {
    use super::*;
    use crate::file_processor::audio::pcm_decode;

    impl AudioDecoderModel {
        pub fn process_audio<T: AsRef<std::path::Path>>(
            &mut self,
            audio_path: T,
        ) -> Result<Vec<Segment>> {
            let mel_bytes = match self.config.num_mel_bins {
                80 => include_bytes!("melfilters.bytes").as_slice(),
                128 => include_bytes!("melfilters128.bytes").as_slice(),
                nmel => anyhow::bail!("unexpected num_mel_bins {nmel}"),
            };
            let mut mel_filters = vec![0f32; mel_bytes.len() / 4];
            <byteorder::LittleEndian as byteorder::ByteOrder>::read_f32_into(
                mel_bytes,
                &mut mel_filters,
            );

            let (pcm_data, sample_rate) = pcm_decode::audio_processing::pcm_decode(audio_path)?;
            if sample_rate != m::SAMPLE_RATE as u32 {
                anyhow::bail!("input file must have a {} sampling rate", m::SAMPLE_RATE)
            }
            println!("pcm data loaded {}", pcm_data.len());
            let mel = audio::pcm_to_mel(&self.config, &pcm_data, &mel_filters);
            let mel_len = mel.len();
            let mel = Tensor::from_vec(
                mel,
                (
                    1,
                    self.config.num_mel_bins,
                    mel_len / self.config.num_mel_bins,
                ),
                &self.device,
            )?;
            println!("loaded mel: {:?}", mel.dims());

            let language_token = None;

            let mut dc = Decoder::new(
                self,
                299792458,
                &self.device.clone(),
                language_token,
                Some(Task::Transcribe),
                false,
                false,
            )?;
            let segments = dc.run(&mel)?;

            Ok(segments)
        }
    }

    impl AudioDecoder for AudioDecoderModel {
        fn decode_audio(&mut self, audio_file: &std::path::Path) -> Result<Vec<Segment>> {
            self.process_audio(audio_file)
        }
    }
}
