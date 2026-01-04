mod actions;

use actions::*;

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use futures_util::StreamExt;
use openaction::*;
use std::collections::HashMap;
use zbus::fdo::DBusProxy;
use zbus::{Connection, MatchRule, MessageStream, MessageType, Proxy};
use zvariant::Value;

async fn fetch_and_convert_to_data_url(url: &str) -> Result<String> {
	let bytes = if url.starts_with("data:") {
		return Ok(url.to_owned());
	} else if url.starts_with("file:") {
		let path = url.trim_start_matches("file://");
		std::fs::read(path)?
	} else {
		let response = reqwest::get(url).await?;
		response.bytes().await?.to_vec()
	};

	let mime_type = infer::get(&bytes)
		.map(|info| info.mime_type())
		.unwrap_or("application/octet-stream");
	let base64_data = general_purpose::STANDARD.encode(&bytes);
	Ok(format!("data:{};base64,{}", mime_type, base64_data))
}

async fn find_active_player(conn: &Connection) -> Result<String> {
	let proxy = Proxy::new(
		conn,
		"org.freedesktop.DBus",
		"/org/freedesktop/DBus",
		"org.freedesktop.DBus",
	)
	.await?;

	let names: Vec<String> = proxy.call("ListNames", &()).await?;
	names
		.into_iter()
		.find(|name| name.starts_with("org.mpris.MediaPlayer2."))
		.ok_or_else(|| anyhow::anyhow!("No MPRIS players found"))
}

async fn get_mpris_proxy() -> Result<Proxy<'static>> {
	let conn = Connection::session().await?;
	let player_name = find_active_player(&conn).await?;

	let proxy = Proxy::new(
		&conn,
		player_name,
		"/org/mpris/MediaPlayer2",
		"org.mpris.MediaPlayer2.Player",
	)
	.await?;

	Ok(proxy)
}

async fn call_mpris_method(method: &str) -> Result<()> {
	let proxy = get_mpris_proxy().await?;
	proxy.call_method(method, &()).await?;
	Ok(())
}

async fn cycle_repeat_mode() -> Result<()> {
	let proxy = get_mpris_proxy().await?;
	let current: String = proxy.get_property("LoopStatus").await?;
	let next = match current.as_str() {
		"None" => "Playlist",
		"Playlist" => "Track",
		"Track" => "None",
		_ => "None",
	};
	proxy.set_property("LoopStatus", next).await?;
	Ok(())
}

async fn toggle_shuffle() -> Result<()> {
	let proxy = get_mpris_proxy().await?;
	let shuffle: bool = proxy.get_property("Shuffle").await?;
	proxy.set_property("Shuffle", &(!shuffle)).await?;
	Ok(())
}

async fn seek(offset_microseconds: i64) -> Result<()> {
	let proxy = get_mpris_proxy().await?;
	proxy.call_method("Seek", &(offset_microseconds,)).await?;
	Ok(())
}

async fn change_volume(instance: &Instance, delta: f64) -> Result<()> {
	let proxy = get_mpris_proxy().await?;

	match proxy.get_property::<f64>("Volume").await {
		Err(zbus::Error::FDO(ref e)) if matches!(e.as_ref(), zbus::fdo::Error::NotSupported(_)) => {
			log::warn!(
				"Volume control not supported by the current MPRIS player: {}",
				e
			);
			let _ = instance.show_alert().await;
			return Ok(());
		}
		Err(e) => {
			log::warn!("Failed to get volume via MPRIS: {}", e);
			let _ = instance.show_alert().await;
			return Err(e.into());
		}
		Ok(current) => {
			let new = (current + delta).clamp(0.0, 1.0);

			if let Err(e) = proxy.set_property("Volume", &new).await {
				log::warn!("Failed to set volume via MPRIS: {}", e);
				let _ = instance.show_alert().await;
				return Err(e.into());
			}
		}
	}

	Ok(())
}

async fn get_album_art(metadata: Option<&Value<'_>>) -> Option<String> {
	fetch_and_convert_to_data_url(
		&metadata?
			.downcast_ref::<zvariant::Dict>()
			.and_then(|dict| dict.get::<str, str>("mpris:artUrl").ok().flatten())
			.map(|url| url.to_string())?,
	)
	.await
	.ok()
}

async fn update_play_pause(instance: &Instance, image: Option<String>) -> OpenActionResult<()> {
	instance.set_image(image, None).await
}

async fn update_repeat(instance: &Instance, loop_status: Option<&Value<'_>>) -> Result<()> {
	let state = match loop_status {
		Some(loop_status_value) => {
			match loop_status_value.downcast_ref::<str>().unwrap_or("None") {
				"None" => 0,
				"Playlist" => 1,
				"Track" => 2,
				_ => 0,
			}
		}
		_ => 0,
	};
	instance.set_state(state).await?;
	Ok(())
}

async fn update_shuffle(instance: &Instance, shuffle: Option<&Value<'_>>) -> Result<()> {
	let state = shuffle
		.and_then(|shuffle_value| shuffle_value.downcast_ref::<bool>().copied())
		.unwrap_or(false);
	instance.set_state(state as u16).await?;
	Ok(())
}

async fn update_all() {
	let proxy_result = get_mpris_proxy().await;
	let get_property = async |property: &str| match &proxy_result {
		Ok(proxy) => proxy.get_property(property).await.ok(),
		Err(_) => None,
	};
	for instance in visible_instances(PlayPauseAction::UUID).await {
		if let Err(error) = update_play_pause(
			&instance,
			get_album_art(get_property("Metadata").await.as_ref()).await,
		)
		.await
		{
			log::error!("Failed to update PlayPause: {}", error);
		}
	}
	for instance in visible_instances(RepeatAction::UUID).await {
		if let Err(error) =
			update_repeat(&instance, get_property("LoopStatus").await.as_ref()).await
		{
			log::error!("Failed to update Repeat: {}", error);
		}
	}
	for instance in visible_instances(ShuffleAction::UUID).await {
		if let Err(error) = update_shuffle(&instance, get_property("Shuffle").await.as_ref()).await
		{
			log::error!("Failed to update Shuffle: {}", error);
		}
	}
}

async fn watch_album_art() {
	let connection = match Connection::session().await {
		Ok(conn) => conn,
		Err(error) => {
			log::error!("Failed to connect to DBus session: {}", error);
			return;
		}
	};

	loop {
		update_all().await;

		let player_name = match find_active_player(&connection).await {
			Ok(name) => name,
			Err(_) => {
				tokio::time::sleep(std::time::Duration::from_secs(1)).await;
				continue;
			}
		};

		let dbus_proxy = match DBusProxy::new(&connection).await {
			Ok(proxy) => proxy,
			Err(error) => {
				log::error!("Failed to create DBus proxy: {}", error);
				return;
			}
		};

		let signal_rule = match MatchRule::builder()
			.msg_type(MessageType::Signal)
			.interface("org.freedesktop.DBus.Properties")
			.and_then(|b| b.member("PropertiesChanged"))
			.and_then(|b| b.path("/org/mpris/MediaPlayer2"))
			.and_then(|b| b.sender(player_name.as_str()))
			.map(|b| b.build())
		{
			Ok(rule) => rule,
			Err(error) => {
				log::error!("Failed to build match rule: {}", error);
				continue;
			}
		};

		if let Err(error) = dbus_proxy.add_match_rule(signal_rule).await {
			log::error!("Failed to add match rule: {}", error);
			continue;
		}

		let name_owner_rule = MatchRule::builder()
			.msg_type(MessageType::Signal)
			.interface("org.freedesktop.DBus")
			.and_then(|b| b.member("NameOwnerChanged"))
			.map(|b| b.build());

		if let Ok(rule) = name_owner_rule {
			let _ = dbus_proxy.add_match_rule(rule).await;
		}

		let mut stream = MessageStream::from(&connection);

		while let Some(msg_result) = stream.next().await {
			let msg = match msg_result {
				Ok(m) => m,
				Err(error) => {
					log::error!("Error receiving message: {}", error);
					continue;
				}
			};

			let header = match msg.header() {
				Ok(h) => h,
				Err(error) => {
					log::error!("Failed to get message header: {}", error);
					continue;
				}
			};

			let member = header.member().ok().flatten().map(|m| m.to_string());
			if member.as_deref() == Some("NameOwnerChanged") {
				if let Ok((name, _old_owner, new_owner)) = msg.body::<(String, String, String)>()
					&& name == player_name
					&& new_owner.is_empty()
				{
					break;
				}
				continue;
			} else if member.as_deref() != Some("PropertiesChanged") {
				continue;
			}

			let body: Result<(String, HashMap<String, Value>, Vec<String>), _> = msg.body();
			let (interface, changed_properties, _) = match body {
				Ok(b) => b,
				Err(error) => {
					log::error!("Error reading message body: {}", error);
					continue;
				}
			};

			if interface != "org.mpris.MediaPlayer2.Player" {
				continue;
			}

			if let Some(playback_status_value) = changed_properties.get("PlaybackStatus")
				&& playback_status_value.downcast_ref::<str>() == Some("Stopped")
			{
				update_all().await;
				continue;
			}

			let album_art_url = get_album_art(changed_properties.get("Metadata")).await;

			if album_art_url.is_some() {
				for instance in visible_instances(PlayPauseAction::UUID).await {
					if let Err(error) = update_play_pause(&instance, album_art_url.clone()).await {
						log::error!("Failed to update PlayPause: {}", error);
					}
				}
			}

			if changed_properties.contains_key("LoopStatus") {
				for instance in visible_instances(RepeatAction::UUID).await {
					if let Err(error) =
						update_repeat(&instance, changed_properties.get("LoopStatus")).await
					{
						log::error!("Failed to update Repeat: {}", error);
					}
				}
			}

			if changed_properties.contains_key("Shuffle") {
				for instance in visible_instances(ShuffleAction::UUID).await {
					if let Err(error) =
						update_shuffle(&instance, changed_properties.get("Shuffle")).await
					{
						log::error!("Failed to update Shuffle: {}", error);
					}
				}
			}
		}
	}
}

#[tokio::main]
async fn main() -> OpenActionResult<()> {
	if let Err(error) = simplelog::TermLogger::init(
		simplelog::LevelFilter::Debug,
		simplelog::Config::default(),
		simplelog::TerminalMode::Stdout,
		simplelog::ColorChoice::Never,
	) {
		eprintln!("Logger initialization failed: {}", error);
	}

	register_action(PlayPauseAction {}).await;
	register_action(StopAction {}).await;
	register_action(PreviousAction {}).await;
	register_action(NextAction {}).await;
	register_action(RepeatAction {}).await;
	register_action(ShuffleAction {}).await;
	register_action(SeekBackwardsAction {}).await;
	register_action(SeekForwardsAction {}).await;
	register_action(VolumeUpAction {}).await;
	register_action(VolumeDownAction {}).await;

	tokio::spawn(watch_album_art());

	run(std::env::args().collect()).await
}
