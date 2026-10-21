# NC-Cable

Cabo de áudio virtual para Linux, escrito em Rust, inspirado no comportamento do
VB-CABLE (Windows). Tudo o que é tocado no dispositivo **"NC-Cable Input"** sai no
dispositivo de gravação **"NC-Cable Output"**.

Uso típico: mandar o áudio de um programa (navegador, player, jogo) como se fosse
microfone para outro (OBS, Discord, DAW, gravador).

```
 app que toca ──► [NC-Cable Input]  (Audio/Sink)
                         │
                   ring buffer (latência configurável)
                         ▼
 app que grava ◄── [NC-Cable Output] (Audio/Source)
```

## Requisitos

- PipeWire (padrão no Arch Linux) com `pipewire-pulse` e WirePlumber.
  Apps PulseAudio, ALSA (`pipewire-alsa`) e JACK (`pipewire-jack`) enxergam o cabo.
- Para compilar: Rust ≥ 1.85, `clang` e os headers do PipeWire (`libpipewire` no Arch).

## Instalação

### Pela release (pacote pronto)

Baixe o pacote da página de *Releases*:

```sh
# Arch Linux
sudo pacman -U nc-cable-0.1.0-1-x86_64.pkg.tar.zst

# Ubuntu 24.04+ (o 22.04 não serve: usa PulseAudio e um PipeWire antigo demais)
sudo apt install ./nc-cable_0.1.0_amd64.deb

# depois, nos dois:
systemctl --user enable --now nc-cable
```

### Arch Linux (compilando)

```sh
cd packaging/arch
makepkg -si
systemctl --user enable --now nc-cable
```

### Gerar os pacotes da release

```sh
scripts/build-release.sh   # cria dist/*.pkg.tar.zst, dist/*.deb e SHA256SUMS (o .deb usa Docker)
```

### Outras distribuições

```sh
./install.sh      # instala em ~/.local/bin e ativa o serviço systemd de usuário
```

### Rodar sem instalar

```sh
cargo run --release
```

## Uso

```sh
nc-cable                    # cria os cabos do arquivo de config (ou um cabo padrão)
nc-cable run --name "Cabo B" --channels 8 --latency 1024
nc-cable status             # contadores e níveis, como o painel do VB-CABLE
nc-cable status --watch
nc-cable init-config        # escreve ~/.config/nc-cable/config.toml de exemplo
```

Depois de rodar, escolha "NC-Cable Input" como saída no app de origem e
"NC-Cable Output" como entrada no app de destino (ou use `pavucontrol`, `qpwgraph`,
`wpctl`).

## Configuração

`~/.config/nc-cable/config.toml`:

```toml
[[cable]]
name = "NC-Cable"
rate = 48000        # 32000 44100 48000 88200 96000 176400 192000
channels = 2        # 1..16
latency = 1536      # amostras entre entrada e saída (mín. 48)
max_latency = 7168  # tamanho do ring buffer (mín. 1024)

[[cable]]           # quantos cabos independentes quiser
name = "NC-Cable B"
```

Mudanças valem após `systemctl --user restart nc-cable`.

### Dicas de latência

- `nc-cable status` avisa quando os buffers do PipeWire são maiores que
  `latency / 3` — mesma regra do driver original. Nesse caso aumente `latency`.
- **Pull loss** = a saída ficou sem dados (aumente `latency`).
  **Push loss** = o buffer encheu (aumente `max_latency`).
- *Resyncs* acontecem quando a fila passa de 2× a latência alvo; o leitor descarta o
  excesso para a latência não crescer.

## Como funciona

Veja [docs/ANALISE.md](docs/ANALISE.md) para a análise do driver original e as
decisões desta implementação.

- `src/ring.rs` — ring buffer lock-free SPSC (f32 intercalado), contadores e picos.
- `src/cable.rs` — cria o par de nós PipeWire e liga os callbacks de tempo real.
- `src/config.rs` — configuração com os mesmos limites do driver.
- `src/stats.rs` — publica estatísticas em `$XDG_RUNTIME_DIR/nc-cable/*.json`.

Testes: `cargo test`.
