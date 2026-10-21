class Dstreamy < Formula
  desc "Stream a window or display to a Discord voice channel"
  homepage "https://github.com/lkshrk/d-streamy"
  version "0.1.0"

  if Hardware::CPU.arm?
    url "https://github.com/lkshrk/d-streamy/releases/download/rs-v#{version}/dstreamy-#{version}-aarch64-apple-darwin.tar.gz"
    sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  else
    url "https://github.com/lkshrk/d-streamy/releases/download/rs-v#{version}/dstreamy-#{version}-x86_64-apple-darwin.tar.gz"
    sha256 "1111111111111111111111111111111111111111111111111111111111111111"
  end

  depends_on macos: :sonoma

  def install
    bin.install "dstreamy"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/dstreamy --version")
  end
end
