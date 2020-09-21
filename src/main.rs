// #![windows_subsystem = "windows"]  // enable to suppress console println!

extern crate tiny_http;

use ascii::AsciiString;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use fltk::{app, button::*, frame::*, window::*};
use futures::prelude::*;
use lazy_static::*;
use rupnp::ssdp::{SearchTarget, URN};
use std::collections::HashMap;
use std::sync::Mutex;
//use std::sync::mpsc::{channel, Receiver, Sender};
use crossbeam_channel::{unbounded, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

mod utils;
use utils::rwstream::ChannelStream;

#[derive(Debug, Clone, Copy)]
pub enum Message {
    Increment,
    Decrement,
}

#[derive(Debug, Clone)]
struct Renderer {
    dev_name: String,
    dev_model: String,
    dev_type: String,
    dev_url: String,
    svc_type: String,
    svc_id: String,
}

#[derive(Debug, Clone, Copy)]
struct WavData {
    sample_format: cpal::SampleFormat,
    sample_rate: cpal::SampleRate,
    channels: u16,
}

macro_rules! DEBUG {
    ($x:stmt) => {
        if cfg!(debug_assertions) {
            $x
        }
    };
}

lazy_static! {
    static ref Clients: Mutex<HashMap<String, ChannelStream>> = Mutex::new(HashMap::new());
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // first initialize cpal audio to prevent COM reinitialize panic on Windows
    let audio_output_device = get_audio_device();
    DEBUG!(eprintln!(
        "Default audio output device: {}",
        audio_output_device.name()?
    ));
    let audio_cfg = &audio_output_device
        .default_output_config()
        .expect("No default output config found");
    DEBUG!(eprintln!("Default config {:?}", audio_cfg));

    let app = app::App::default().with_scheme(app::Scheme::Gleam);
    let (sw, sh) = app::screen_size();
    let mut wind = Window::default()
        .with_size((sw as i32) / 3, (sh as i32) / 3)
        .with_label("UPNP/DLNA Renderers");

    let fw = (sw as i32) / 4;
    let fx = ((wind.width() - 30) / 2) - (fw / 2);
    let mut frame = Frame::new(fx, 5, fw, 30, "").with_align(Align::Center);
    frame.set_frame(FrameType::BorderBox);

    let local_addr = get_local_addr().expect("Could not obtain local address.");
    frame.set_label(&format!(
        "Scanning {} for UPNP rendering devices",
        local_addr
    ));
    wind.make_resizable(true);
    wind.end();
    wind.show();
    for _ in 1..100 {
        app::wait_for(0.001)?
    }

    // build a list with renderers descovered on the network
    let renderers = discover().await?;
    // Event handling channel
    let (s, r) = app::channel::<i32>();
    // the buttons with the discovered renderers
    let mut buttons: Vec<LightButton> = Vec::new();
    // now create a button for each renderer
    let bwidth = frame.width() / 2; // button width
    let bheight = frame.height(); // button height
    let bx = ((wind.width() - 30) / 2) - (bwidth / 2); // button x offset
    let mut by = frame.y() + frame.height() + 10; // button y offset
    let mut bi = 0; // button index
    let mut rs: Vec<Renderer> = Vec::new();
    match renderers {
        Some(rends) => {
            rs = rends;
            for renderer in rs.iter() {
                let mut but = LightButton::default() // create the button
                    .with_size(bwidth, bheight)
                    .with_pos(bx, by)
                    .with_align(Align::Center)
                    .with_label(&format!("{} {}", renderer.dev_model, renderer.dev_name));
                but.emit(s, bi); // button click events arrive on a channel with the button index as message data
                wind.add(&but); // add the button to the window
                buttons.push(but); // and keep a reference to it
                bi += 1; // bump the button index
                by += bheight + 10; // and the button y offset
            }
        }
        None => {}
    }
    frame.set_label("Rendering Devices");
    wind.redraw();

    // capture system audio
    let stream = capture_output_audio();
    stream.play().expect("Could not play audio capture stream");

    // start webserver
    let wd = WavData {
        sample_format: audio_cfg.sample_format(),
        sample_rate: audio_cfg.sample_rate(),
        channels: audio_cfg.channels(),
    };

    let _ = std::thread::spawn(move || run_server(&local_addr, wd));

    while app.wait()? {
        match r.recv() {
            Some(i) => {
                // a button has been clicked
                let b = &buttons[i as usize]; // get a reference to the button that was clicked
                let renderer = &rs[i as usize];
                DEBUG!(eprintln!(
                    "Pushed renderer {} {}, state = {}",
                    renderer.dev_model,
                    renderer.dev_name,
                    if b.is_on() { "ON" } else { "OFF" }
                ));
            }
            None => (),
        }
    }
    Ok(())
}

fn run_server(local_addr: &IpAddr, wd: WavData) -> () {
    let addr = format!("{}:{}", local_addr, 5901);
    DEBUG!(eprintln!("Serving on {}", addr));
    let server = Arc::new(tiny_http::Server::http(addr).unwrap());

    let mut handles = Vec::new();

    for _ in 0..4 {
        let server = server.clone();

        handles.push(thread::spawn(move || {
            for rq in server.incoming_requests() {
                DEBUG!(eprintln!(
                    "Reveived request {} from {}",
                    rq.url(),
                    rq.remote_addr()
                ));
                let remote_addr = format!("{}", rq.remote_addr());
                let (tx, rx): (Sender<u16>, Receiver<u16>) = unbounded();
                let channel_stream = ChannelStream {
                    s: tx.clone(),
                    r: rx.clone(),
                };
                let mut clients = Clients.lock().unwrap();
                clients.insert(remote_addr.clone(), channel_stream);
                drop(clients);
                let channel_stream = ChannelStream {
                    s: tx.clone(),
                    r: rx.clone(),
                };
                //                let s = std::fs::File::open("example.txt").unwrap();
                let ct = tiny_http::Header {
                    field: "Content-Type".parse().unwrap(),
                    value: AsciiString::from_ascii("text/xml").unwrap(),
                };
                let response = tiny_http::Response::empty(200).with_header(ct);
                let response = response.with_data(channel_stream, None);
                let _ = rq.respond(response);
                let mut clients = Clients.lock().unwrap();
                clients.remove(&remote_addr);
                drop(clients);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
}

fn capture_output_audio() -> cpal::Stream {
    // first initialize cpal audio to prevent COM reinitialize panic on Windows
    let audio_output_device = get_audio_device();
    DEBUG!(eprintln!(
        "Default audio output device: {}",
        audio_output_device
            .name()
            .expect("Could not get default audio device name")
    ));
    let audio_cfg = &audio_output_device
        .default_output_config()
        .expect("No default output config found");
    DEBUG!(eprintln!("Default config {:?}", audio_cfg));

    let wd = WavData {
        sample_format: audio_cfg.sample_format(),
        sample_rate: audio_cfg.sample_rate(),
        channels: audio_cfg.channels(),
    };

    let stream = match audio_cfg.sample_format() {
        cpal::SampleFormat::F32 => {
            let (tx, rx): (Sender<f32>, Receiver<f32>) = unbounded();
            let s = audio_output_device
                .build_input_stream(
                    &audio_cfg.config(),
                    move |data, _: &_| wave_reader::<f32>(tx.clone(), data),
                    err_fn,
                )
                .expect("Could not capture f32 stream format");
            let _ = std::thread::spawn(move || {
                wave_writer(rx);
            });
            s
        }
        cpal::SampleFormat::I16 => {
            let (tx, rx): (Sender<i16>, Receiver<i16>) = unbounded();
            let s = audio_output_device
                .build_input_stream(
                    &audio_cfg.config(),
                    move |data, _: &_| wave_reader::<i16>(tx.clone(), data),
                    err_fn,
                )
                .expect("Could not capture i16 stream format");
            let _ = std::thread::spawn(move || {
                wave_writer(rx);
            });
            s
        }
        cpal::SampleFormat::U16 => {
            let (tx, rx): (Sender<u16>, Receiver<u16>) = unbounded();
            let s = audio_output_device
                .build_input_stream(
                    &audio_cfg.config(),
                    move |data, _: &_| wave_reader::<u16>(tx.clone(), data),
                    err_fn,
                )
                .expect("Could not capture u16 stream format");
            let _ = std::thread::spawn(move || {
                wave_writer(rx);
            });
            s
        }
    };
    stream
}

fn err_fn(err: cpal::StreamError) {
    eprintln!("Error {} building audio input stream", err);
}

fn wave_reader<T>(s: Sender<T>, samples: &[T])
where
    T: cpal::Sample,
{
    DEBUG!(eprintln!("received {} samples", samples.len()));
    for &sample in samples.iter() {
        s.send(sample).ok();
    }
}

fn wave_writer<T>(r: Receiver<T>) -> ()
where
    T: cpal::Sample,
{
    let clients = Clients.lock().unwrap();
    let mut channels = vec![];
    for (_, client) in clients.iter() {
        channels.push(client.s.clone());
    }
    drop(clients);

    for sample in r.iter() {
        let dest_sample = sample.to_u16();
        for channel in channels.iter() {
            channel.send(dest_sample).unwrap();
        }
    }
}

///
/// discover the available (audio) renderers on the network
///  
async fn discover() -> Result<Option<Vec<Renderer>>, rupnp::Error> {
    const RENDERING_CONTROL: URN = URN::service("schemas-upnp-org", "RenderingControl", 1);

    if cfg!(debug_assertions) {
        println!("Starting SSDP renderer discovery");
    }

    let mut renderers: Vec<Renderer> = Vec::new();
    let search_target = SearchTarget::URN(RENDERING_CONTROL);
    match rupnp::discover(&search_target, Duration::from_secs(3)).await {
        Ok(d) => {
            pin_utils::pin_mut!(d);
            loop {
                if let Some(device) = d.try_next().await? {
                    if device.services().len() > 0 {
                        if let Some(service) = device.find_service(&RENDERING_CONTROL) {
                            DEBUG!(print_renderer(&device, &service));
                            renderers.push(Renderer {
                                dev_name: device.friendly_name().to_string(),
                                dev_model: device.model_name().to_string(),
                                dev_type: device.device_type().to_string(),
                                dev_url: device.url().to_string(),
                                svc_id: service.service_type().to_string(),
                                svc_type: service.service_type().to_string(),
                            });
                            /*
                                                let args = "<InstanceID>0</InstanceID><Channel>Master</Channel>";
                                                match service.action(device.url(), "GetVolume", args).await {
                                                    Ok(response) => {
                                                        println!("Got response from {}", device.friendly_name());
                                                        let volume = response.get("CurrentVolume").expect("Error getting volume");
                                                        println!("'{}' is at volume {}", device.friendly_name(), volume);
                                                    }
                                                    Err(err) => {
                                                        println!("Error '{}' in GetVolume", err);
                                                    }
                                                }
                            */
                        }
                    } else {
                        DEBUG!(eprintln!(
                            "*No services* type={}, manufacturer={}, name={}, model={}, at url= {}",
                            device.device_type(),
                            device.manufacturer(),
                            device.friendly_name(),
                            device.model_name(),
                            device.url()
                        ));
                    }
                } else {
                    DEBUG!(eprintln!("End of SSDP devices discovery"));
                    break;
                }
            }
        }
        Err(e) => {
            eprintln!("Error {} running SSDP discover", e);
        }
    }

    Ok(Some(renderers))
}

///
/// print the information for a renderer
///
fn print_renderer(device: &rupnp::Device, service: &rupnp::Service) {
    eprintln!(
        "Found renderer type={}, manufacturer={}, name={}, model={}, at url= {}",
        device.device_type(),
        device.manufacturer(),
        device.friendly_name(),
        device.model_name(),
        device.url()
    );
    eprintln!(
        "  Service type: {}, id:   {}",
        service.service_type(),
        service.service_id()
    );
}

///
/// return the default output audio device
///
fn get_audio_device() -> cpal::Device {
    // audio hosts
    DEBUG!(eprintln!("Supported audio hosts: {:?}", cpal::ALL_HOSTS));
    let available_hosts = cpal::available_hosts();
    DEBUG!(eprintln!("Available audio hosts: {:?}", available_hosts));
    let default_host = cpal::default_host();
    let default_device = default_host
        .default_output_device()
        .expect("Failed to get the default audio output device");
    default_device
}

use std::net::{IpAddr, UdpSocket};

/// get the local ip address, return an `Option<String>`. when it fails, return `None`.
fn get_local_addr() -> Option<IpAddr> {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return None,
    };

    match socket.connect("8.8.8.8:80") {
        Ok(()) => (),
        Err(_) => return None,
    };

    match socket.local_addr() {
        Ok(addr) => return Some(addr.ip()),
        Err(_) => return None,
    };
}
