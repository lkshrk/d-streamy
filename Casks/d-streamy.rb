cask "d-streamy" do
  version "0.9.0"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  url "https://github.com/lkshrk/d-streamy/releases/download/v#{version}/D-Streamy.zip"
  name "D-Streamy"
  desc "Menu bar app that streams any macOS window to a Discord voice channel"
  homepage "https://github.com/lkshrk/d-streamy"

  # Native daemon addons ship as darwin-arm64 only.
  depends_on arch: :arm64
  depends_on macos: :sonoma

  app "D-Streamy.app"

  zap trash: [
    "~/Library/Preferences/me.harke.d-streamy.plist",
    "~/Library/Logs/D-Streamy",
  ]
end
