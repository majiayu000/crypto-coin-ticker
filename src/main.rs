use std::time::Duration;

use exc::prelude::*;
use futures::StreamExt;
use tracing_subscriber::prelude::*;
use std::thread::sleep;

use chrono::Local;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::{
    menu::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    TrayIconBuilder, TrayIconEvent,
};
use tokio::task;

use std::sync::mpsc::{channel, Receiver, Sender};


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fmt = tracing_subscriber::fmt::layer()
    .with_writer(std::io::stderr)
    .with_filter(tracing_subscriber::EnvFilter::new(
        std::env::var("RUST_LOG").unwrap_or_else(|_| "exc_okx=debug,okx_streams=debug".into()),
    ));
tracing_subscriber::registry().with(fmt).init();

let exchange = Okx::endpoint()
    .ws_ping_timeout(Duration::from_secs(5))
    .ws_connection_timeout(Duration::from_secs(2))
    .connect_exc();

let (tx, rx) = channel();  // 创建一个channel

// 克隆发送端用于任务
let tx_clone = tx.clone();

let handles = ["BTC-USDT"]
    .into_iter()
    .map(|inst| {
        let mut client = exchange.clone();
        let tx = tx_clone.clone();  // 在这里克隆tx
        tokio::spawn(async move {
            loop {
                tracing::info!("{inst}");
                match { client.subscribe_tickers(inst).await } {
                    Ok(mut stream) => {
                        while let Some(c) = stream.next().await {
                            match c {
                                Ok(c) => {
                                    tracing::info!("{0}", c.last);
                                    tx.send(c.last).unwrap_or_else(|_| tracing::warn!("Failed to send data to channel"));
                                },
                                Err(err) => {
                                    tracing::error!("{err}");
                                }
                            }
                        }
                        tracing::warn!("stream is dead; reconnecting..");
                    },
                    Err(err) => {
                        tracing::error!("request error: {err}; retrying..");
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        })
    })
    .collect::<Vec<_>>();

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/icons/icon.png");
    // println!("{}", &path);
    // for h in handles {
    //     let _ = h.await;
    // }



    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/icons/icon.png");
    println!("{}", &path);
    let icon = load_icon(std::path::Path::new(path));

    let event_loop = EventLoopBuilder::new().build();

    let tray_menu = Menu::new();

    let quit_i = MenuItem::new("Quit", true, None);
    tray_menu.append_items(&[


        &PredefinedMenuItem::separator(),
        &quit_i,
    ]);

    let mut tray_icon = Some(
        TrayIconBuilder::new()
            .with_id("1")
            .with_menu(Box::new(tray_menu))
            .with_title("ss")
            .with_tooltip("tao - awesome windowing lib")
            .with_icon(icon)
            .build()
            .unwrap(),
    );


    // let mut icon = TrayIcon;

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();
    let current_time = Local::now().format("%H:%M:%S").to_string();
    
    event_loop.run(move |_event, _, control_flow| {
        *control_flow = ControlFlow::Poll;
        
        
        let current_time = Local::now().format("%H:%M:%S").to_string();

        // std::thread::sleep(std::time::Duration::from_secs(3));
     
        // if let Some(ref mut tray) = tray_icon {
        //     tray.set_title(Some(&current_time));
        // }

        if let Ok(price) = rx.try_recv() {
            let price_str = format!("BTC-USDT: {}", price);
            if let Some(ref mut tray) = tray_icon {
                tray.set_title(Some(&price_str));
            }
        }

        // if let Ok(price) = rx.try_recv() {
        //     let price_str = format!("BTC-USDT: {}", price);
        //     if let Some(ref mut tray) = tray_icon {
        //         tray.set_title(Some(&current_time));
        //     }
        // }

        if let Ok(event) = menu_channel.try_recv() {
            if event.id == quit_i.id() {
                tray_icon.take();
             

                *control_flow = ControlFlow::Exit;
            }
            
            println!("{event:?}");
        }

        if let Ok(event) = tray_channel.try_recv() {
            println!("{event:?}");
            // tray_channel.set_title("a");
        }
    });

    
    Ok(())

 
}



fn load_icon(path: &std::path::Path) -> tray_icon::Icon {
    let (icon_rgba, icon_width, icon_height) = {
        let image = image::open(path)
            .expect("Failed to open icon path")
            .into_rgba8();
        let (width, height) = image.dimensions();
        let rgba = image.into_raw();
        (rgba, width, height)
    };
    tray_icon::Icon::from_rgba(icon_rgba, icon_width, icon_height).expect("Failed to open icon")
}