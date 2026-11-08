#!/usr/bin/env sh
# Instala (ou atualiza) o Acervo no Linux, só para o seu usuário. Não precisa de sudo.
#
#   curl -fsSL https://raw.githubusercontent.com/gabrielc2bral/acervo/main/scripts/install-linux.sh | sh
#
# Para remover:  sh install-linux.sh --remover
set -eu

REPO="gabrielc2bral/acervo"
DEST="${XDG_DATA_HOME:-$HOME/.local/share}/acervo"
BIN="$HOME/.local/bin"
APPS="${XDG_DATA_HOME:-$HOME/.local/share}/applications"

if [ "${1:-}" = "--remover" ]; then
    rm -rf "$DEST" "$BIN/acervo" "$BIN/acervo-cli" "$APPS/acervo.desktop"
    echo "Acervo removido. Seu catálogo continua em ~/.local/share/Acervo."
    exit 0
fi

[ "$(uname -m)" = "x86_64" ] || { echo "Só há pacote para x86_64 (64 bits Intel/AMD)." >&2; exit 1; }
command -v curl >/dev/null 2>&1 || { echo "Instale o curl primeiro." >&2; exit 1; }
command -v sha256sum >/dev/null 2>&1 || { echo "Não achei o sha256sum." >&2; exit 1; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Procurando a última versão..."
url="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | grep -o '"browser_download_url": *"[^"]*linux-x86_64\.tar\.gz"' \
    | head -n 1 | sed 's/.*"\(https[^"]*\)"/\1/')"
[ -n "$url" ] || { echo "Não achei um pacote para Linux no último release." >&2; exit 1; }

file="$tmp/acervo.tar.gz"
echo "Baixando $url"
curl -fL --progress-bar -o "$file" "$url"
curl -fsSL -o "$tmp/hash.txt" "${url%.tar.gz}.sha256.txt"

# Confere o SHA-256 publicado no release antes de usar o arquivo.
esperado="$(cut -d' ' -f1 "$tmp/hash.txt")"
atual="$(sha256sum "$file" | cut -d' ' -f1)"
[ "$esperado" = "$atual" ] || { echo "O SHA-256 não confere. Nada foi instalado." >&2; exit 1; }

mkdir -p "$DEST" "$BIN" "$APPS"
tar -xzf "$file" -C "$tmp"
pasta="$(find "$tmp" -maxdepth 1 -type d -name 'Acervo-*' | head -n 1)"
cp "$pasta/acervo" "$pasta/acervo-cli" "$pasta/acervo.svg" "$DEST/"
chmod +x "$DEST/acervo" "$DEST/acervo-cli"
ln -sf "$DEST/acervo" "$BIN/acervo"
ln -sf "$DEST/acervo-cli" "$BIN/acervo-cli"

cat > "$APPS/acervo.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Acervo
Comment=Catálogo rápido dos seus arquivos e HDs
Exec=$DEST/acervo
Icon=$DEST/acervo.svg
Terminal=false
Categories=Utility;FileTools;
EOF

echo "Pronto. Abra o Acervo pelo menu de aplicativos ou rode: acervo"
case ":$PATH:" in
    *":$BIN:"*) ;;
    *) echo "Aviso: $BIN não está no seu PATH. Adicione-o ao ~/.profile para usar o comando 'acervo'." ;;
esac
