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

  def caveats
    <<~EOS
      onto-intelli stores its data in PostgreSQL 16 or newer with pgvector:
        brew install postgresql@16 pgvector
      or run the bundled docker-compose.yml (`make db-up`), then
        export ONTO_DATABASE_URL=postgres://user:pass@127.0.0.1:5432/db
    EOS
  end

  test do
    system bin/"onto", "init", testpath
    # Data lives in PostgreSQL; the in-memory store is enough to check the binary.
    inreplace testpath/"onto-intelli.toml", 'storage = "postgres"', 'storage = "memory"'
    cfg = "--config #{testpath}/onto-intelli.toml"
    assert_match "TicketService", shell_output("#{bin}/onto #{cfg} protos")
    assert_match "Seeded workspace acme", shell_output("#{bin}/onto #{cfg} seed demo")
  end
end
