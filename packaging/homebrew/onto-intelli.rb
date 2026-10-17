# Homebrew formula template. Publish in a tap (e.g. f8fvwgzc/homebrew-tap) and update
# `url`/`sha256` per release:  brew install f8fvwgzc/tap/onto-intelli
class OntoIntelli < Formula
  desc "Ontology-based Enterprise Execution Intelligence: gRPC, memory, context graphs, LLM"
  homepage "https://github.com/f8fvwgzc/onto-intelli"
  url "https://github.com/f8fvwgzc/onto-intelli/archive/refs/tags/v0.2.0.tar.gz"
  sha256 "REPLACE_WITH_RELEASE_TARBALL_SHA256"
  license "MIT"
  head "https://github.com/f8fvwgzc/onto-intelli.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: "crates/onto-cli")
  end

  test do
    system bin/"onto", "init", testpath
    assert_match "Payments Team", shell_output("#{bin}/onto --config #{testpath}/onto-intelli.toml seed demo && " \
                                               "#{bin}/onto --config #{testpath}/onto-intelli.toml ask 'Who owns Project Alpha?'")
  end
end
