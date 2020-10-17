mod commands;
mod helpers;
mod structures;
mod reactions;

use serenity::{
    async_trait,
    framework::standard::{
        StandardFramework,
        CommandError,
        DispatchError,
        macros::hook
    },
    http::Http,
    model::{
        prelude::{
            Permissions,
            Message, User
        },
        event::VoiceServerUpdateEvent, 
        gateway::Ready, 
        guild::{Member, Guild}, 
        channel::Reaction, 
        id::GuildId
    },
    prelude::*, 
    client::bridge::gateway::GatewayIntents,
    framework::standard::CommandResult
};
use std::{
    collections::{
        HashSet,
        HashMap
    },
    env, 
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering}
    }
};
use lavalink_rs::{
    gateway::*, 
    LavalinkClient
};
use futures::future::AbortHandle;
use dashmap::DashMap;
use reqwest::Client as Reqwest;

use structures::{
    cmd_data::*,
    commands::*,
    errors::*
};
use helpers::{database_helper, delete_buffer, command_utils};
use reactions::reaction_handler;

// Event handler for when the bot starts
struct Handler {
    run_loop: AtomicBool
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _: Context, ready: Ready) {
        println!("Connected as {}", ready.user.name);
    }

    async fn cache_ready(&self, ctx: Context, _guilds: Vec<GuildId>) {
        if self.run_loop.load(Ordering::Relaxed) {
            self.run_loop.store(false, Ordering::Relaxed);

            println!("Starting starboard deletion loop!");
            tokio::spawn(async move {
                if let Err(e) = delete_buffer::starboard_removal_loop(ctx).await {
                    panic!("Delete buffer failed to start!: {}", e);
                };
            });
        }
    }

    async fn guild_create(&self, ctx: Context, guild: Guild, is_new: bool) {
        let pool = ctx.data.read().await
            .get::<ConnectionPool>().cloned().unwrap();

        if is_new {
            sqlx::query!("INSERT INTO guild_info VALUES($1, null) ON CONFLICT DO NOTHING", guild.id.0 as i64)
                .execute(&pool).await.unwrap();
        }
    }

    async fn guild_member_removal(&self, ctx: Context, guild_id: GuildId, user: User, _member_data_if_available: Option<Member>) {
        let (bot_id, pool) = {
            let data = ctx.data.read().await;
            let bot_id = data.get::<BotId>().cloned().unwrap();
            let pool = data.get::<ConnectionPool>().cloned().unwrap();

            (bot_id, pool)
        };

        if user.id == bot_id {    
            sqlx::query!("DELETE FROM guild_info WHERE guild_id = $1", guild_id.0 as i64)
                .execute(&pool).await.unwrap();  
        }
    }

    async fn reaction_add(&self, ctx: Context, reaction: Reaction) {
        let _ = reaction_handler::dispatch_reaction(&ctx, &reaction, false).await;
    }

    async fn reaction_remove(&self, ctx: Context, reaction: Reaction) {
        let _ = reaction_handler::dispatch_reaction(&ctx, &reaction, true).await;
    }

    async fn voice_server_update(&self, ctx: Context, voice: VoiceServerUpdateEvent) {
        if let Some(guild_id) = voice.guild_id {
            let voice_server_lock = ctx.data.read().await
                .get::<VoiceGuildUpdate>().cloned().unwrap();
            let mut voice_server = voice_server_lock.write().await;
            voice_server.insert(guild_id);
        }
    }
}

struct LavalinkHandler;

#[async_trait]
impl LavalinkEventHandler for LavalinkHandler {}

#[tokio::main]
async fn main() -> CommandResult {
    pretty_env_logger::init();
    
    let args: Vec<String> = env::args().collect();
    let creds = helpers::credentials_helper::read_creds(&args[1])?;
    let token = &creds.bot_token;

    let http = Http::new_with_token(&token);

    let (owners, bot_id) = match http.get_current_application_info().await {
        Ok(info) => {
            let mut owners = HashSet::new();
            owners.insert(info.owner.id);

            (owners, info.id)
        },
        Err(why) => panic!("Could not access application info: {:?}", why),
    };

    let pool = database_helper::obtain_db_pool(creds.db_connection).await?;
    let prefixes = database_helper::fetch_prefixes(&pool).await?;
    let voice_timer_map: DashMap<GuildId, AbortHandle> = DashMap::new(); 

    let mut lava_client = LavalinkClient::new(bot_id);
    lava_client.set_host(creds.lavalink_host);
    lava_client.set_password(creds.lavalink_auth);
    let lava = lava_client.initialize(LavalinkHandler).await?;

    let mut pub_creds = HashMap::new();
    pub_creds.insert("tenor".to_string(), creds.tenor_key);
    pub_creds.insert("default prefix".to_string(), creds.default_prefix);

    let emergency_commands = command_utils::get_allowed_commands();

    let command_names = MASTER_GROUP.options.sub_groups.iter().flat_map(|x| {
        x.options.commands
            .iter()
            .flat_map(|i| i.options.names.iter().map(ToString::to_string))
            .collect::<Vec<_>>()
    }).collect::<Vec<String>>();

    let reqwest_client = Reqwest::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:73.0) Gecko/20100101 Firefox/73.0")
        .build()?;

    // If there's no command, check in the custom commands database
    #[hook]
    async fn unrecognized_command_hook(ctx: &Context, msg: &Message, command_name: &str) {
        let pool = ctx.data.read().await
            .get::<ConnectionPool>().cloned().unwrap();

        let guild_id = msg.guild_id.unwrap().0 as i64;

        let cmd_data = sqlx::query!(
                "SELECT content FROM commands WHERE guild_id = $1 AND name = $2",
                guild_id, command_name)
            .fetch_optional(&pool).await.unwrap();

        if let Some(cmd_data) = cmd_data {
            let content = cmd_data.content.unwrap()
                .replace("{user}", &msg.author.mention());
            let _ = msg.channel_id.say(ctx, content).await;
        }
    }

    #[hook]
    async fn before(ctx: &Context, msg: &Message, cmd_name: &str) -> bool {
        if command_utils::check_mention_prefix(msg) {
            let emergency_commands = ctx.data.read().await
                .get::<EmergencyCommands>().cloned().unwrap();

            if emergency_commands.contains(&cmd_name.to_owned()) {
                let _ = msg.channel_id.say(ctx, 
                    format!("{}, you are running an emergency command!", msg.author.mention())).await;
                return true
            } else {
                return false
            }
        }

        true
    }

    // After a command is executed, goto here
    #[hook]
    async fn after(ctx: &Context, msg: &Message, cmd_name: &str, error: Result<(), CommandError>) {
        if let Err(why) = error {
            let part_1 = "Looks like the bot encountered an error! \n";
            let part_2 = "Please use the `support` command and send the output to the support server!";
            let error_string = format!("{}{}", part_1, part_2);

            let _ = msg.channel_id.send_message(ctx, |m| {
                m.embed(|e| {
                    e.color(0xff69b4);
                    e.title("Aw Snap!");
                    e.description(error_string);
                    e.field("Command Name", cmd_name, false);
                    e.field("Error", format!("```{} \n```", why), false);
                    e
                })
            }).await;
        }
    }

    // On a dispatch error, go to this function
    #[hook]
    async fn dispatch_error(ctx: &Context, msg: &Message, error: DispatchError) {
        match error {
            DispatchError::LackingPermissions(Permissions::ADMINISTRATOR) => {
                let _ = msg.channel_id.say(ctx, 
                    JesterError::PermissionError(PermissionType::SelfPerm("administrator"))).await;
            },
            DispatchError::LackingPermissions(Permissions::MANAGE_MESSAGES) => {
                let _ = msg.channel_id.say(ctx, 
                    JesterError::PermissionError(PermissionType::SelfPerm("moderator"))).await;
            },
            DispatchError::NotEnoughArguments { min, given } => {
                let _ = msg.channel_id.say(ctx, format!("Args required: {}. Args given: {}", min, given)).await;
            },
            DispatchError::OnlyForOwners => {
                let _ = msg.channel_id.say(ctx, "This is a bot dev only command!").await;
            },
            _ => println!("Unhandled dispatch error: {:?}", error),
        }        
    }

    /*
     * The heart of custom prefixes
     * If the guild has a prefix in the Dashmap, use that prefix
     * Otherwise, use the default prefix from credentials_helper
     */

    #[hook]
    async fn dynamic_prefix(ctx: &Context, msg: &Message) -> Option<String> {
        let (prefixes, default_prefix) = {
            let data = ctx.data.read().await;
            let prefixes = data.get::<PrefixMap>().cloned().unwrap();
            let default_prefix = data.get::<PubCreds>().unwrap()
                .get("default prefix").cloned().unwrap();

            (prefixes, default_prefix)
        };

        let guild_id = msg.guild_id.unwrap();
 
        match prefixes.get(&guild_id) {
            Some(prefix_guard) => Some(prefix_guard.value().to_owned()),
            None => Some(default_prefix)
        }
    }

    // Link everything together!
    let framework = StandardFramework::new()
        .configure(|c| c
            .dynamic_prefix(dynamic_prefix)
            .on_mention(Some(bot_id))
            .owners(owners)
        )

        .on_dispatch_error(dispatch_error)
        .unrecognised_command(unrecognized_command_hook)
        .before(before)
        .after(after)
        
        .group(&GENERAL_GROUP)
        .group(&TEXT_GROUP)
        .group(&TEXTLAST_GROUP)
        .group(&CIPHERS_GROUP)
        .group(&TEXTCHANNELSEND_GROUP)
        .group(&CONFIG_GROUP)
        .group(&SUPPORT_GROUP)
        .group(&STARBOARD_GROUP)
        .group(&VOICE_GROUP)
        .group(&MUSIC_GROUP)
        .group(&IMAGES_GROUP)
        .group(&JAPAN_GROUP);

    let mut client = Client::builder(&token)
        .framework(framework)
        .event_handler(Handler { run_loop: AtomicBool::new(true) })
        .add_intent({
            let mut intents = GatewayIntents::all();
            intents.remove(GatewayIntents::DIRECT_MESSAGES);
            intents.remove(GatewayIntents::DIRECT_MESSAGE_REACTIONS);
            intents.remove(GatewayIntents::DIRECT_MESSAGE_TYPING);
            intents
        })
        .await
        .expect("Err creating client");

    {
        // Insert all structures into ctx data
        let mut data = client.data.write().await;

        data.insert::<ConnectionPool>(pool.clone());
        data.insert::<ShardManagerContainer>(Arc::clone(&client.shard_manager));
        data.insert::<Lavalink>(lava);
        data.insert::<VoiceGuildUpdate>(Arc::new(RwLock::new(HashSet::new())));
        data.insert::<VoiceManager>(Arc::clone(&client.voice_manager));
        data.insert::<VoiceTimerMap>(Arc::new(voice_timer_map));
        data.insert::<PrefixMap>(Arc::new(prefixes));
        data.insert::<CommandNameMap>(Arc::new(command_names));
        data.insert::<ReqwestClient>(reqwest_client);
        data.insert::<PubCreds>(Arc::new(pub_creds));
        data.insert::<EmergencyCommands>(Arc::new(emergency_commands));
        data.insert::<BotId>(bot_id);
    }

    let _owners = match client.cache_and_http.http.get_current_application_info().await {
        Ok(info) => {
            let mut set = HashSet::new();
            set.insert(info.owner.id);

            set
        },
        Err(why) => panic!("Couldn't get application info: {:?}", why),
    };

    // Start up the bot! If there's an error, let the user know
    if let Err(why) = client.start_autosharded().await {
        eprintln!("Client error: {:?}", why);
    }

    Ok(())
}
