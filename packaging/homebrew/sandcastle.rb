# Homebrew cask for sandcastle: the app, with the CLI linked onto the PATH.
#
# This file is the template. The live copy belongs in a tap repository
# (github.com/cburkhal/homebrew-tap, Casks/sandcastle.rb), so that
#   brew install --cask cburkhal/tap/sandcastle
# works. For each release, set `version` and `sha256` (printed by the release
# workflow) in the tap's copy.
cask "sandcastle" do
  version "<version>"
  sha256 "<sha256 of the dmg>"

  url "https://github.com/cburkhal/sandcastle/releases/download/v#{version}/sandcastle_#{version}_aarch64.dmg"
  name "sandcastle"
  desc "One OpenCode instance per Git repo, each in its own Apple container VM"
  homepage "https://github.com/cburkhal/sandcastle"

  depends_on arch: :arm64
  depends_on macos: ">= :tahoe"
  depends_on formula: "bitwarden-cli"

  app "sandcastle.app"
  binary "#{appdir}/sandcastle.app/Contents/MacOS/sandcastle"

  zap trash: [
    "~/.config/sandcastle",
    "~/Library/Application Support/sandcastle",
  ]

  caveats <<~EOS
    sandcastle also needs Apple's `container`, which is not in Homebrew:
      https://github.com/apple/container/releases
    Then run `sandcastle init` once and `sandcastle doctor` to check the setup.
    `brew uninstall` leaves the box image, networks and volumes in `container`,
    and the line `anchor "sandcastle/*"` in /etc/pf.conf.
  EOS
end
