cask "d-streamy" do
  version "0.9.0"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  url "https://github.com/lkshrk/d-streamy/releases/download/v#{version}/D-Streamy-#{version}-macos-universal.zip"
  name "D-Streamy"
  desc "Menu bar app that streams any macOS window to a Discord voice channel"
  homepage "https://github.com/lkshrk/d-streamy"

  depends_on macos: :sonoma

  app "D-Streamy.app"

  # The dstreamy CLI and the OBS plugin share this config dir, so zap removes their accounts too.
  zap trash: [
    "~/Library/Application Support/me.harke.dstreamy",
    "~/Library/Preferences/me.harke.d-streamy.plist",
  ]
end
