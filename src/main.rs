use std::{
    collections::HashMap, io::Write, str::FromStr, sync::Arc, thread::sleep, time::Duration,
};

use clap::clap_app;

use anyhow::anyhow;
use ipnetwork::IpNetwork;
use tokio::sync::RwLock;

use crate::{
    authority::{find_members, init_trust_dns_authority, new_ptr_authority},
    utils::update_central_dns,
};

mod authority;
mod hosts;
mod server;
mod supervise;
mod utils;

#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod tests;

fn write_help(app: clap::App) -> Result<(), anyhow::Error> {
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    app.clone().write_long_help(&mut lock)?;
    writeln!(lock)?;
    return Ok(());
}

fn unsupervise(network: Option<&str>) -> Result<(), anyhow::Error> {
    supervise::Properties::new(None, network, None, None, None)?.uninstall_supervisor()
}

fn supervise(
    domain: Option<&str>,
    network: Option<&str>,
    hosts_file: Option<&str>,
    authtoken: Option<&str>,
    token: Option<&str>,
) -> Result<(), anyhow::Error> {
    supervise::Properties::new(domain, network, hosts_file, authtoken, token)?.install_supervisor()
}

fn start(
    domain: Option<&str>,
    network: Option<&str>,
    hosts_file: Option<&str>,
    authtoken: Option<&str>,
    token: Option<&str>,
    wildcard_names: bool,
) -> Result<(), anyhow::Error> {
    let domain_name = utils::domain_or_default(domain)?;
    let authtoken = utils::authtoken_path(authtoken);
    let runtime = &mut utils::init_runtime();

    if let Some(network) = network {
        let token = utils::central_token(token);
        let network = String::from(network);
        let hf = if let Some(hf) = hosts_file {
            Some(hf.to_string())
        } else {
            None
        };

        if token.is_none() {
            return Err(anyhow!("missing zerotier central token: set ZEROTIER_CENTRAL_TOKEN in environment, or pass a file containing it with -t"));
        }

        let token = token.unwrap();

        println!("Welcome to ZeroNS!");

        let ips = runtime.block_on(utils::get_listen_ips(&authtoken, &network))?;

        if ips.len() > 0 {
            update_central_dns(
                runtime,
                domain_name.clone(),
                utils::parse_ip_from_cidr(ips.clone().into_iter()
                    .find(|i| IpNetwork::from_str(i).expect("Could not parse CIDR").is_ipv4())
                    .expect("Could not find a valid IPv4 network (currently, ipv6 resolvers are unsupported)")),
                token.clone(),
                network.clone(),
            )?;

            let mut listen_ips = Vec::new();
            let mut ipmap = HashMap::new();
            let mut authority_map = HashMap::new();
            let authority = init_trust_dns_authority(domain_name.clone());

            for cidr in ips.clone() {
                let listen_ip = utils::parse_ip_from_cidr(cidr.clone());
                listen_ips.push(listen_ip.clone());
                let cidr = IpNetwork::from_str(&cidr.clone())?;
                if !ipmap.contains_key(&listen_ip) {
                    ipmap.insert(listen_ip, cidr.network());
                }

                if !authority_map.contains_key(&cidr.network()) {
                    let ptr_authority = new_ptr_authority(cidr)?;

                    let mut ztauthority = utils::init_authority(
                        ptr_authority.clone(),
                        token.clone(),
                        network.clone(),
                        domain_name.clone(),
                        hf.clone(),
                        Duration::new(30, 0),
                        authority.clone(),
                    );

                    if wildcard_names {
                        ztauthority.wildcard_everything();
                    }

                    let arc_authority = Arc::new(RwLock::new(ztauthority));

                    authority_map.insert(cidr.network(), arc_authority.clone());
                    runtime.spawn(find_members(arc_authority));
                }
            }

            for ip in listen_ips {
                println!("Your IP for this network: {}", ip);
                let cidr = ipmap
                    .get(&ip)
                    .expect("Could not locate underlying network subnet");
                let authority = authority_map
                    .get(cidr)
                    .expect("Could not locate PTR authority for subnet");

                let server = crate::server::Server::new(authority.to_owned());
                runtime.spawn(server.listen(format!("{}:53", ip.clone()), Duration::new(0, 1000)));
            }

            async fn wait() {
                loop {
                    sleep(Duration::new(60, 0))
                }
            }

            return Ok(runtime.block_on(wait()));
        }

        return Err(anyhow!(
            "No listening IPs for your interface; assign one in ZeroTier Central."
        ));
    }

    return Err(anyhow!("no network ID"));
}

fn main() -> Result<(), anyhow::Error> {
    let app = clap::clap_app!(zeronsd =>
        (author: "Erik Hollensbe <github@hollensbe.org>")
        (about: "zerotier central nameserver")
        (version: utils::VERSION_STRING)
        (@subcommand start =>
            (about: "Start the nameserver")
            (@arg domain: -d --domain +takes_value "TLD to use for hostnames")
            (@arg file: -f --file +takes_value "An additional lists of hosts in /etc/hosts format")
            (@arg secret_file: -s --secret +takes_value "Path to authtoken.secret (usually detected)")
            (@arg token_file: -t --token +takes_value "Path to a file containing the ZeroTier Central token")
            (@arg wildcard_names: -w --wildcard-names "Wildcard all names in Central to point at the respective member's IP address(es)")
            (@arg NETWORK_ID: +required "Network ID to query")
        )
        (@subcommand supervise =>
            (about: "Configure supervision of the nameserver for a single network")
            (@arg domain: -d --domain +takes_value "TLD to use for hostnames")
            (@arg file: -f --file +takes_value "An additional lists of hosts in /etc/hosts format")
            (@arg secret_file: -s --secret +takes_value "Path to authtoken.secret (usually detected)")
            (@arg token_file: -t --token +takes_value +required "Path to a file containing the ZeroTier Central token; this file must not be moved")
            (@arg NETWORK_ID: +required "Network ID to query")
        )
        (@subcommand unsupervise =>
            (about: "Remove supervision of the nameserver for a network")
            (@arg NETWORK_ID: +required "Network ID to remove")
        )
    );

    let matches = app.clone().get_matches().clone();

    let (cmd, args) = matches.subcommand();
    let args = match args {
        Some(args) => args,
        None => return write_help(app),
    };

    match cmd {
        "start" => start(
            args.value_of("domain"),
            args.value_of("NETWORK_ID"),
            args.value_of("file"),
            args.value_of("secret_file"),
            args.value_of("token_file"),
            args.is_present("wildcard_names"),
        )?,
        "supervise" => supervise(
            args.value_of("domain"),
            args.value_of("NETWORK_ID"),
            args.value_of("file"),
            args.value_of("secret_file"),
            args.value_of("token_file"),
        )?,
        "unsupervise" => unsupervise(args.value_of("NETWORK_ID"))?,
        _ => {
            let stderr = std::io::stderr();
            let mut lock = stderr.lock();
            app.clone()
                .write_long_help(&mut lock)
                .expect("Could not write help to stdio: Welp.");
            writeln!(lock)?;
            return Ok(());
        }
    }

    Ok(())
}
