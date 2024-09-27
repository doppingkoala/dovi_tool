use anyhow::{bail, ensure, Result};
use hdr10plus::metadata::{PeakBrightnessSource, VariablePeakBrightness};
use hdr10plus::metadata_json::MetadataJsonRoot;
use std::fs::File;
use std::io::{stdout, Write};
use std::path::{Path, PathBuf};

use crate::commands::GenerateArgs;
use dolby_vision::rpu::extension_metadata::blocks::{
    ExtMetadataBlock, ExtMetadataBlockLevel1, ExtMetadataBlockLevel6,
};
use dolby_vision::rpu::generate::{GenerateConfig, GenerateProfile, ShotFrameEdit, VideoShot};
use dolby_vision::utils::nits_to_pq;
use dolby_vision::xml::{CmXmlParser, XmlParserOpts};

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratorProfile {
    #[value(name = "5")]
    Profile5,
    #[value(name = "8.1")]
    Profile81,
    #[value(name = "8.4")]
    Profile84,
}

#[derive(Default)]
pub struct Generator {
    json_path: Option<PathBuf>,
    rpu_out: PathBuf,
    hdr10plus_path: Option<PathBuf>,
    hdr10plus_peak_source: Option<PeakBrightnessSource>,
    xml_path: Option<PathBuf>,
    canvas_width: Option<u16>,
    canvas_height: Option<u16>,
    madvr_path: Option<PathBuf>,
    use_custom_targets: bool,
    profile: Option<GeneratorProfile>,
    long_play_mode: Option<bool>,

    pub config: Option<GenerateConfig>,
}

impl Generator {
    pub fn from_args(args: GenerateArgs) -> Result<Generator> {
        let GenerateArgs {
            json_file,
            rpu_out,
            hdr10plus_json,
            hdr10plus_peak_source,
            xml,
            canvas_width,
            canvas_height,
            madvr_file,
            use_custom_targets,
            profile,
            long_play_mode,
        } = args;

        let out_path = if let Some(out_path) = rpu_out {
            out_path
        } else {
            PathBuf::from("RPU_generated.bin".to_string())
        };

        let generator = Generator {
            json_path: json_file,
            rpu_out: out_path,
            hdr10plus_path: hdr10plus_json,
            hdr10plus_peak_source: hdr10plus_peak_source.map(From::from),
            xml_path: xml,
            canvas_width,
            canvas_height,
            madvr_path: madvr_file,
            use_custom_targets,
            profile,
            config: None,
            long_play_mode,
        };

        Ok(generator)
    }

    pub fn generate(args: GenerateArgs) -> Result<()> {
        let mut generator = Generator::from_args(args)?;
        generator.execute()
    }

    pub fn execute(&mut self) -> Result<()> {
        let mut config = if let Some(json_path) = &self.json_path {
            let json_file = File::open(json_path)?;

            println!("Reading generate config file...");
            let mut config: GenerateConfig = serde_json::from_reader(&json_file)?;

            // Set default to the config's CM version if it wasn't specified
            config.l1_avg_pq_cm_version.get_or_insert(config.cm_version);

            if let Some(hdr10plus_path) = &self.hdr10plus_path {
                let peak_source = self
                    .hdr10plus_peak_source
                    .as_ref()
                    .expect("Missing required DR10+ peak source");
                parse_hdr10plus_for_l1(hdr10plus_path, *peak_source, &mut config)?;
            } else if let Some(madvr_path) = &self.madvr_path {
                generate_metadata_from_madvr(madvr_path, self.use_custom_targets, &mut config)?;
            } else if config.length == 0 && !config.shots.is_empty() {
                // Set length from sum of shot durations
                config.length = config.shots.iter().map(|s| s.duration).sum();
            }

            ensure!(
                config.length > 0 || !config.shots.is_empty(),
                "Missing number of RPUs to generate, and no shots to derive it from"
            );

            // Create a single shot by default
            if config.shots.is_empty() {
                config.shots.push(VideoShot {
                    start: 0,
                    duration: config.length,
                    ..Default::default()
                })
            }

            config
        } else if let Some(xml_path) = &self.xml_path {
            self.config_from_xml(xml_path)?
        } else {
            bail!("Missing configuration or XML file!");
        };

        // Override config with manual arg
        if let Some(profile) = self.profile {
            config.profile = GenerateProfile::from(profile);
        }

        if let Some(long_play_mode) = self.long_play_mode {
            config.long_play_mode = long_play_mode
        }

        self.config = Some(config);

        if let Some(config) = self.config.as_mut() {
            println!("Generating metadata: {}...", &config.profile);

            // Correct L1 for sources other than XML
            if self.xml_path.is_none() {
                config.fixup_l1();
            }

            config.write_rpus(&self.rpu_out)?;

            println!("Generated metadata for {} frames", config.length);
        } else {
            bail!("No generation config to execute!");
        }

        println!("Done.");

        Ok(())
    }

    fn config_from_xml<P: AsRef<Path>>(&self, xml_path: P) -> Result<GenerateConfig> {
        println!("Parsing XML metadata...");

        let parser_opts = XmlParserOpts {
            canvas_width: self.canvas_width,
            canvas_height: self.canvas_height,
        };

        let parser = CmXmlParser::parse_file(xml_path, parser_opts)?;

        Ok(parser.config)
    }
}

fn parse_hdr10plus_for_l1<P: AsRef<Path>>(
    hdr10plus_path: P,
    peak_source: PeakBrightnessSource,
    config: &mut GenerateConfig,
) -> Result<()> {
    println!("Parsing HDR10+ JSON file...");
    stdout().flush().ok();

    let metadata_root = MetadataJsonRoot::from_file(&hdr10plus_path)?;

    let frame_count = metadata_root.scene_info.len();

    let mut scene_first_frames = metadata_root.scene_info_summary.scene_first_frame_index;
    let first_frame_index = scene_first_frames
        .first()
        .cloned()
        .expect("Missing SceneFirstFrameIndex array");

    // Offset indices according to first index, since they should start at 0
    scene_first_frames
        .iter_mut()
        .for_each(|i| *i -= first_frame_index);

    let scene_frame_lengths = metadata_root.scene_info_summary.scene_frame_numbers;

    let mut hdr10plus_shots = Vec::with_capacity(scene_first_frames.len());

    let first_frames = metadata_root
        .scene_info
        .iter()
        .enumerate()
        .filter(|(frame_no, _)| scene_first_frames.contains(frame_no));

    for (current_shot_id, (frame_no, frame_meta)) in first_frames.enumerate() {
        let max_nits = frame_meta.peak_brightness_nits(PeakBrightnessSource::Histogram).unwrap();

        let min_pq = 0;
        let max_pq = (nits_to_pq(max_nits.round()) * 4095.0).round() as u16;
        let avg_pq;
        if frame_meta.luminance_parameters.luminance_distributions.distribution_index.len() == 9 {
            if frame_meta.luminance_parameters.luminance_distributions.distribution_index[1] == 5 && frame_meta.luminance_parameters.luminance_distributions.distribution_index[2] == 10{
                let pq1 = nits_to_pq(frame_meta.luminance_parameters.luminance_distributions.distribution_values[0] as f64 / 10.0);
                let pq2 = nits_to_pq(frame_meta.luminance_parameters.luminance_distributions.distribution_values[3] as f64 / 10.0);
                let pq3 = nits_to_pq(frame_meta.luminance_parameters.luminance_distributions.distribution_values[4] as f64 / 10.0);
                let pq4 = nits_to_pq(frame_meta.luminance_parameters.luminance_distributions.distribution_values[5] as f64 / 10.0);
                let pq5 = nits_to_pq(frame_meta.luminance_parameters.luminance_distributions.distribution_values[6] as f64 / 10.0);
                let pq6 = nits_to_pq(frame_meta.luminance_parameters.luminance_distributions.distribution_values[7] as f64 / 10.0);
                let pq7 = nits_to_pq(frame_meta.luminance_parameters.luminance_distributions.distribution_values[8] as f64 / 10.0);
                let mean_pq =  (pq1 + pq2) / 2.0 * 0.2400 +
                                (pq2 + pq3) / 2.0 * 0.2500 +
                                (pq3 + pq4) / 2.0 * 0.2500 +
                                (pq4 + pq5) / 2.0 * 0.1500 +
                                (pq5 + pq6) / 2.0 * 0.0500 +
                                (pq6 + pq7) / 2.0 * 0.0498;
                avg_pq = (mean_pq * 4095.0).round() as u16;
            } else {
                let avg_nits = frame_meta.luminance_parameters.average_rgb as f64 / 10.0;
                avg_pq = (nits_to_pq(avg_nits.round()) * 4095.0).round() as u16;
            }
        } else {
            let avg_nits = frame_meta.luminance_parameters.average_rgb as f64 / 10.0;
            avg_pq = (nits_to_pq(avg_nits.round()) * 4095.0).round() as u16;
        }

        let mut shot = VideoShot {
            start: frame_no,
            duration: scene_frame_lengths[current_shot_id],
            metadata_blocks: vec![ExtMetadataBlock::Level1(
                ExtMetadataBlockLevel1::from_stats_cm_version(
                    min_pq,
                    max_pq,
                    avg_pq,
                    config.l1_avg_pq_cm_version.unwrap(),
                ),
            )],
            ..Default::default()
        };

        let config_shot = config.shots.get(hdr10plus_shots.len());

        if let Some(override_shot) = config_shot {
            shot.copy_metadata_from_shot(override_shot, Some(&[1]))
        }

        hdr10plus_shots.push(shot);
    }

    // Now that the metadata was copied, we can replace the shots
    config.shots.clear();
    config.shots.extend(hdr10plus_shots);

    config.length = frame_count;

    Ok(())
}

pub fn generate_metadata_from_madvr<P: AsRef<Path>>(
    madvr_path: P,
    use_custom_targets: bool,
    config: &mut GenerateConfig,
) -> Result<()> {
    println!("Parsing madVR measurement file...");
    stdout().flush().ok();

    let madvr_info = madvr_parse::MadVRMeasurements::parse_file(madvr_path)?;

    let level6_meta = ExtMetadataBlockLevel6 {
        max_content_light_level: madvr_info.header.maxcll as u16,
        max_frame_average_light_level: madvr_info.header.maxfall as u16,
        ..Default::default()
    };

    let frame_count = madvr_info.frames.len();
    let mut madvr_shots = Vec::with_capacity(madvr_info.scenes.len());

    for (i, scene) in madvr_info.scenes.iter().enumerate() {
        let min_pq = 0;
        let max_pq = (scene.max_pq * 4095.0).round() as u16;
        let avg_pq = (scene.avg_pq * 4095.0).round() as u16;

        let mut shot = VideoShot {
            start: scene.start as usize,
            duration: scene.length,
            metadata_blocks: vec![ExtMetadataBlock::Level1(
                ExtMetadataBlockLevel1::from_stats_cm_version(
                    min_pq,
                    max_pq,
                    avg_pq,
                    config.l1_avg_pq_cm_version.unwrap(),
                ),
            )],
            ..Default::default()
        };

        let config_shot = config.shots.get(i);

        if use_custom_targets && madvr_info.header.flags == 3 {
            // Use peak per frame, average from scene
            let frames = scene.get_frames(frame_count, &madvr_info.frames)?;

            frames.iter().enumerate().for_each(|(i, f)| {
                let min_pq = 0;
                let max_pq = (f.target_pq * 4095.0).round() as u16;
                let avg_pq = (scene.avg_pq * 4095.0).round() as u16;

                let frame_edit = ShotFrameEdit {
                    edit_offset: i,
                    metadata_blocks: vec![ExtMetadataBlock::Level1(
                        ExtMetadataBlockLevel1::from_stats_cm_version(
                            min_pq,
                            max_pq,
                            avg_pq,
                            config.l1_avg_pq_cm_version.unwrap(),
                        ),
                    )],
                };

                shot.frame_edits.push(frame_edit);
            });
        }

        if let Some(override_shot) = config_shot {
            shot.copy_metadata_from_shot(override_shot, Some(&[1]))
        }

        madvr_shots.push(shot);
    }

    // Now that the metadata was copied, we can replace the shots
    config.shots.clear();
    config.shots.extend(madvr_shots);

    // Set MaxCLL and MaxFALL if not set in config
    if let Some(config_l6) = config.level6.as_mut() {
        if config_l6.max_content_light_level == 0 {
            config_l6.max_content_light_level = level6_meta.max_content_light_level;
        }

        if config_l6.max_frame_average_light_level == 0 {
            config_l6.max_frame_average_light_level = level6_meta.max_frame_average_light_level;
        }
    }

    config.length = frame_count;

    Ok(())
}

impl From<GeneratorProfile> for GenerateProfile {
    fn from(p: GeneratorProfile) -> Self {
        match p {
            GeneratorProfile::Profile5 => GenerateProfile::Profile5,
            GeneratorProfile::Profile81 => GenerateProfile::Profile81,
            GeneratorProfile::Profile84 => GenerateProfile::Profile84,
        }
    }
}
