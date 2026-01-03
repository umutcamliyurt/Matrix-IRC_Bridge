# Matrix-IRC_Bridge

## A minimalist bridge between IRC and Matrix protocols

<!-- REQUIREMENTS -->
## Requirements:

- Rust

- A Matrix account with an access token and access to the rooms that will be bridged

- IRC server

<!-- CONFIGURATION -->
## Configuration

This bridge is configured **entirely via environment variables**.  
Using a `.env` file is recommended (supported via `dotenv`).

---

These variables define how the bridge authenticates and connects to Matrix.

    MATRIX_ACCESS_TOKEN=your_matrix_access_token
    MATRIX_USER_ID=@bridge:example.org
    MATRIX_DEVICE_ID=BRIDGE_DEVICE
    MATRIX_HOMESERVER=https://example.org
    IRC_SERVER=irc.example.net
    IRC_PORT=6697
    IRC_USE_TLS=true
    IRC_SERVER_PASSWORD=your_irc_password

### Bridge Mappings (Required)

    BRIDGE_1_MATRIX_ROOM_ID=!roomid1:example.org
    BRIDGE_1_IRC_CHANNEL=#channel1

    BRIDGE_2_MATRIX_ROOM_ID=!roomid2:example.org
    BRIDGE_2_IRC_CHANNEL=#channel2



<!-- INSTALLATION -->
## Installation:
    
    git clone https://github.com/umutcamliyurt/Matrix-IRC_Bridge.git
    cd Matrix-IRC_Bridge/
    cargo build
    TOKIO_WORKER_THREADS=4
    cargo run
