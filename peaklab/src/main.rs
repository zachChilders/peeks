use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use peaklab::dem::Dem;
use peaklab::geo::{self, Geodetic};
use peaklab::{data_dir, EYE_HEIGHT_M};

#[derive(Parser)]
#[command(name = "peaklab", about = "AR peak identification, desktop harness")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sample the DEM at a point (M0).
    Elev {
        #[arg(long, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        lon: f64,
    },
    /// Look angles from an observer to a target (M1).
    ///
    /// Both altitudes default to the DEM surface; the observer additionally gets eye
    /// height added.
    Bearing {
        #[arg(long, allow_hyphen_values = true)]
        from_lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        from_lon: f64,
        #[arg(long, allow_hyphen_values = true)]
        to_lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        to_lon: f64,
        /// Override the observer altitude (metres, ellipsoidal).
        #[arg(long)]
        from_alt: Option<f64>,
        /// Override the target altitude (metres, ellipsoidal).
        #[arg(long)]
        to_alt: Option<f64>,
    },
    /// Fetch named OSM peaks around a point and snap them to the DEM (M2).
    Peaks {
        #[arg(long, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        lon: f64,
        #[arg(long, default_value_t = 150.0)]
        radius_km: f64,
        /// Snap search radius in DEM postings (1 posting ≈ 30 m).
        ///
        /// Measured against 344 peaks' tagged `ele` (see peaks.rs docs): OSM node
        /// placement is already accurate (median |Δ| ~11 m at window=0), and widening
        /// the window mostly climbs onto neighbouring terrain rather than fixing
        /// anything — the count of peaks reading >50 m *above* their tagged elevation
        /// roughly triples from window=0 to window=240 m. Default is deliberately small.
        #[arg(long, default_value_t = 1)]
        snap: i64,
        /// Show the N highest peaks found.
        #[arg(long, default_value_t = 15)]
        top: usize,
        /// Also dump the full resolved list as JSON.
        #[arg(long)]
        json: Option<std::path::PathBuf>,
    },
    /// Fetch peaks and filter to what's actually visible from a point (M3).
    Scan {
        #[arg(long, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        lon: f64,
        /// Observer altitude override (metres, ellipsoidal). Defaults to DEM + eye height.
        #[arg(long)]
        alt: Option<f64>,
        #[arg(long, default_value_t = 150.0)]
        radius_km: f64,
        #[arg(long, default_value_t = 1)]
        snap: i64,
        #[arg(long, default_value_t = 60.0)]
        step_m: f64,
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// List occluded peaks too (only the first `top`, nearest first).
        #[arg(long)]
        show_occluded: bool,
    },
    /// Project visible peaks onto an image and label them (M4).
    Render {
        #[arg(long, allow_hyphen_values = true)]
        lat: f64,
        #[arg(long, allow_hyphen_values = true)]
        lon: f64,
        /// Observer altitude override (metres, ellipsoidal). Defaults to DEM + eye height.
        #[arg(long)]
        alt: Option<f64>,
        #[arg(long, default_value_t = 150.0)]
        radius_km: f64,
        #[arg(long, default_value_t = 1)]
        snap: i64,
        #[arg(long, default_value_t = 60.0)]
        step_m: f64,

        /// True-north azimuth the camera is pointed at, clockwise, degrees.
        #[arg(long, allow_hyphen_values = true)]
        yaw: f64,
        /// Degrees, up positive.
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        pitch: f64,
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        roll: f64,
        #[arg(long, default_value_t = 66.0)]
        hfov: f64,

        /// Background photo to draw on. Omit to render onto a synthetic sky gradient,
        /// which still validates the projection/layout math.
        #[arg(long)]
        photo: Option<std::path::PathBuf>,
        /// Canvas size when no --photo is given.
        #[arg(long, default_value_t = 1200)]
        width: u32,
        #[arg(long, default_value_t = 900)]
        height: u32,

        #[arg(long)]
        font: Option<std::path::PathBuf>,
        #[arg(long, default_value = "render.png")]
        out: std::path::PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dem_dir = data_dir().join("dem");

    match cli.command {
        Command::Elev { lat, lon } => {
            let mut dem = Dem::new(&dem_dir);
            dem.load_region(lat, lon, 1_000.0)?;

            let bilinear = dem
                .elevation_at(lat, lon)
                .context("no DEM coverage at that point")?;
            let nearest = dem.elevation_nearest(lat, lon).unwrap();
            let (_, _, peak_elev) = dem.local_max(lat, lon, 2).unwrap();

            println!("lat/lon      {lat:.6}, {lon:.6}");
            println!("bilinear     {bilinear:.2} m  ({:.1} ft)", bilinear * 3.28084);
            println!("nearest      {nearest:.2} m");
            println!("5x5 max      {peak_elev:.2} m");
        }

        Command::Bearing {
            from_lat,
            from_lon,
            to_lat,
            to_lon,
            from_alt,
            to_alt,
        } => {
            let mut dem = Dem::new(&dem_dir);
            let radius = geo::great_circle_distance(
                Geodetic::new(from_lat, from_lon, 0.0),
                Geodetic::new(to_lat, to_lon, 0.0),
            );
            dem.load_region(from_lat, from_lon, radius + 2_000.0)?;

            let observer = Geodetic::new(
                from_lat,
                from_lon,
                match from_alt {
                    Some(a) => a,
                    None => {
                        dem.elevation_at(from_lat, from_lon)
                            .context("no DEM coverage at the observer")?
                            + EYE_HEIGHT_M
                    }
                },
            );
            let target = Geodetic::new(
                to_lat,
                to_lon,
                match to_alt {
                    Some(a) => a,
                    None => dem
                        .elevation_at(to_lat, to_lon)
                        .context("no DEM coverage at the target")?,
                },
            );

            let v = geo::enu(observer, target);
            let (az, elev, slant) = geo::look_angles(observer, target);
            let geometric = geo::elevation_deg(v);
            let surface = geo::great_circle_distance(observer, target);

            println!("observer     {:.6}, {:.6}  @ {:.1} m", observer.lat, observer.lon, observer.alt);
            println!("target       {:.6}, {:.6}  @ {:.1} m", target.lat, target.lon, target.alt);
            println!();
            println!("azimuth      {az:.4}°  (true north, clockwise)");
            println!("elevation    {elev:.4}°  (geometric {geometric:.4}° + refraction {:.4}°)",
                     elev - geometric);
            println!("surface dist {:.1} m  ({:.2} km)", surface, surface / 1000.0);
            println!("slant dist   {:.1} m", slant);
            println!("ENU          E {:.1}  N {:.1}  U {:.1}", v[0], v[1], v[2]);
        }

        Command::Peaks {
            lat,
            lon,
            radius_km,
            snap,
            top,
            json,
        } => {
            let mut dem = Dem::new(&dem_dir);
            let mut peaks = peaklab::peaks::load(
                &data_dir(),
                &mut dem,
                lat,
                lon,
                radius_km * 1000.0,
                snap,
            )?;

            let (tiles, mib) = dem.resident();
            println!("{} named peaks within {radius_km:.0} km", peaks.len());
            println!("{tiles} DEM tiles resident ({mib:.0} MiB)\n");

            let mut offsets: Vec<f64> = peaks.iter().map(|p| p.snap_offset_m).collect();
            offsets.sort_by(|a, b| a.partial_cmp(b).unwrap());
            if !offsets.is_empty() {
                let pct = |q: f64| offsets[((offsets.len() - 1) as f64 * q) as usize];
                println!(
                    "snap offset  median {:.0} m   p90 {:.0} m   max {:.0} m",
                    pct(0.5),
                    pct(0.9),
                    offsets[offsets.len() - 1]
                );
                let pinned = offsets.iter().filter(|o| **o > (snap as f64 * 30.0) * 0.9).count();
                println!(
                    "{pinned} peaks snapped to the window edge (window is {:.0} m)\n",
                    snap as f64 * 30.0
                );
            }

            peaks.sort_by(|a, b| b.elev.partial_cmp(&a.elev).unwrap());
            println!("{:<28} {:>9} {:>9} {:>8}", "name", "dem_m", "osm_ele", "snap_m");
            for p in peaks.iter().take(top) {
                println!(
                    "{:<28} {:>9.0} {:>9} {:>8.0}",
                    truncate(&p.name, 28),
                    p.elev,
                    p.osm_ele.map(|e| format!("{e:.0}")).unwrap_or_else(|| "-".into()),
                    p.snap_offset_m
                );
            }

            if let Some(path) = json {
                std::fs::write(&path, serde_json::to_string_pretty(&peaks)?)?;
                println!("\nwrote {} peaks to {}", peaks.len(), path.display());
            }
        }

        Command::Scan {
            lat,
            lon,
            alt,
            radius_km,
            snap,
            step_m,
            top,
            show_occluded,
        } => {
            use peaklab::visibility::{self, VisibilityConfig};

            let mut dem = Dem::new(&dem_dir);
            let peaks = peaklab::peaks::load(&data_dir(), &mut dem, lat, lon, radius_km * 1000.0, snap)?;

            let observer = Geodetic::new(
                lat,
                lon,
                match alt {
                    Some(a) => a,
                    None => {
                        dem.elevation_at(lat, lon)
                            .context("no DEM coverage at the observer")?
                            + EYE_HEIGHT_M
                    }
                },
            );

            let cfg = VisibilityConfig {
                step_m,
                ..Default::default()
            };

            let mut visible = Vec::new();
            let mut occluded = Vec::new();
            let mut unknown = 0;

            for p in &peaks {
                let target = Geodetic::new(p.lat, p.lon, p.elev);
                let (az, elev_angle, _) = geo::look_angles(observer, target);
                let dist = geo::great_circle_distance(observer, target);
                match visibility::check(&dem, observer, target, cfg) {
                    peaklab::visibility::Visibility::Visible => {
                        visible.push((p, az, elev_angle, dist))
                    }
                    peaklab::visibility::Visibility::Occluded { at_dist_m, .. } => {
                        occluded.push((p, az, elev_angle, dist, at_dist_m))
                    }
                    peaklab::visibility::Visibility::Unknown => unknown += 1,
                }
            }

            println!(
                "observer     {lat:.6}, {lon:.6} @ {:.1} m\n",
                observer.alt
            );
            println!(
                "{} peaks fetched, {} visible, {} occluded, {} unknown (DEM gap)\n",
                peaks.len(),
                visible.len(),
                occluded.len(),
                unknown
            );

            visible.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
            println!(
                "-- visible (by elevation angle) --\n{:<26} {:>7} {:>7} {:>9}",
                "name", "az", "elev°", "dist_km"
            );
            for (p, az, elev, dist) in visible.iter().take(top) {
                println!(
                    "{:<26} {:>7.1} {:>7.2} {:>9.1}",
                    truncate(&p.name, 26),
                    az,
                    elev,
                    dist / 1000.0
                );
            }

            if show_occluded {
                occluded.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap());
                println!(
                    "\n-- occluded (nearest first) --\n{:<26} {:>7} {:>9} {:>12}",
                    "name", "az", "dist_km", "blocked_km"
                );
                for (p, az, _, dist, at) in occluded.iter().take(top) {
                    println!(
                        "{:<26} {:>7.1} {:>9.1} {:>12.1}",
                        truncate(&p.name, 26),
                        az,
                        dist / 1000.0,
                        at / 1000.0
                    );
                }
            }
        }

        Command::Render {
            lat,
            lon,
            alt,
            radius_km,
            snap,
            step_m,
            yaw,
            pitch,
            roll,
            hfov,
            photo,
            width,
            height,
            font,
            out,
        } => {
            use peaklab::projection::CameraPose;
            use peaklab::render::{self, Candidate};
            use peaklab::visibility::{self, VisibilityConfig};

            let mut dem = Dem::new(&dem_dir);
            let peaks =
                peaklab::peaks::load(&data_dir(), &mut dem, lat, lon, radius_km * 1000.0, snap)?;

            let observer = Geodetic::new(
                lat,
                lon,
                match alt {
                    Some(a) => a,
                    None => {
                        dem.elevation_at(lat, lon)
                            .context("no DEM coverage at the observer")?
                            + EYE_HEIGHT_M
                    }
                },
            );

            let mut canvas = match &photo {
                Some(path) => image::open(path)
                    .with_context(|| format!("opening {}", path.display()))?
                    .to_rgba8(),
                None => render::blank_canvas(width, height),
            };
            let (w, h) = canvas.dimensions();

            let cam = CameraPose {
                yaw_deg: yaw,
                pitch_deg: pitch,
                roll_deg: roll,
                hfov_deg: hfov,
                width: w,
                height: h,
            };

            let vis_cfg = VisibilityConfig {
                step_m,
                ..Default::default()
            };

            // Margin lets a label attach to a peak whose dot is just off-frame.
            const MARGIN: f64 = 60.0;
            let mut onscreen: Vec<(f64, Candidate)> = Vec::new(); // (distance, candidate)
            let mut visible_count = 0;

            for p in &peaks {
                let target = Geodetic::new(p.lat, p.lon, p.elev);
                if !matches!(
                    visibility::check(&dem, observer, target, vis_cfg),
                    visibility::Visibility::Visible
                ) {
                    continue;
                }
                visible_count += 1;

                let v = geo::enu(observer, target);
                let Some((x, y)) = cam.project(v) else {
                    continue;
                };
                if x < -MARGIN || x > w as f64 + MARGIN || y < -MARGIN || y > h as f64 + MARGIN {
                    continue;
                }

                let dist = geo::great_circle_distance(observer, target);
                onscreen.push((
                    dist,
                    Candidate {
                        label: p.name.clone(),
                        pixel: (x, y),
                    },
                ));
            }
            onscreen.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            let candidates: Vec<Candidate> = onscreen.into_iter().map(|(_, c)| c).collect();

            let font_bytes = render::load_font(font.as_deref())?;
            let placed = render::draw_labels(&mut canvas, &candidates, &font_bytes)?;

            canvas.save(&out).with_context(|| format!("saving {}", out.display()))?;

            let labeled = placed.iter().filter(|p| p.text_rect.is_some()).count();
            println!(
                "{} peaks fetched, {} visible, {} in frame, {} labeled (vfov {:.1}°)",
                peaks.len(),
                visible_count,
                candidates.len(),
                labeled,
                cam.vfov_deg(),
            );
            println!("wrote {}", out.display());
        }
    }

    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).chain("…".chars()).collect()
    }
}
