# ScreenExtend
> Extend your screen. Extend your possibilities. Unlock ultimate productivity.

A free, cross-platform desktop-extension solution that transforms any device into a second monitor. Features at a glance:
- Runs completely in browser on the client, without a need for downloading additional apps
- Password-protected sessions prevent unauthorized devices from joining
- Offline mode allows usage anytime, anywhere
- Supports Windows, Mac, and Linux systems

Built with Rust and Typescript.

## Under the Hood
The frontend is built using React.js and ShadCN UI. The interface connects to the Rust backend using Tauri. It uses virtual displays and WebRTC to simulate an extended monitor. The app also has a local account system; no application data is stored on ScreenExtend servers.

## Copyright and License
This project is licensed using the GNU Affero GPL. Any code from ScreenExtend that is incorporated in other projects must include the original copyright notice and license text. All code must remain public and accessible to users. Any changes made to the code must be clearly indicated. Developers can freely contribute code to the main repository via pull requests. Inquiries should be sent to [hi@screenextend.app](mailto:hi@screenextend.app).

## Contributing
ScreenExtend is limited to a subset of platforms; if your platform is unsupported, email [support@screenextend.app](mailto:support@screenextend.app) with your device information for an appropriate installer. Submit issues for any feature requests or bugs. For code contributions, open a pull request. Pull requests and issues will be reviewed on a biweekly basis.
