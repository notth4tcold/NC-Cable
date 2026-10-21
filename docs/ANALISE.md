# Análise do VB-CABLE Driver Pack 45

Alvo: `VBCABLE_Driver_Pack45` (driver 3.3.1.7, 07/10/2024). Analisado o binário
`vbaudio_cable64_win10.sys` (SHA-256 `f0134460…31d5`) com Hopper via REA, além
dos `.inf`, `readme.txt` e strings do `VBCABLE_ControlPanel.exe`.
Nenhum código do VB-CABLE foi copiado; o NC-Cable é uma implementação nova do
mesmo comportamento.

## O que o pacote contém

| Arquivo | Papel |
|---|---|
| `vbaudio_cable*_{xp,vista,win7,2003,win10}.sys` | Driver de kernel PortCls/KS (x86, x64, ARM64) |
| `vbMmeCable*.inf` / `.cat` | Instalação e assinatura |
| `VBCABLE_Setup*.exe` | Instalador |
| `VBCABLE_ControlPanel.exe` | Painel: latência, SR interno, contadores, níveis |

## Observado no binário

**Endpoints** (INF + strings): um endpoint de reprodução *"CABLE Input"* (`Out1_Wave`/`Out1_Topo`),
um de gravação *"CABLE Output"* (`In1_Wave`/`In1_Topo`) e, no Win10, *"CABLE In 16 Ch"*.
Formato padrão do motor de áudio: 48 kHz, 24 bits, estéreo (`PKEY_AudioEngine_OEMFormat`).

**Parâmetros** (função de inicialização em `0x1400164e9`), lidos de
`HKLM\Software\VB-Audio\Cable` e, se ausentes, de `...\Services\VB-Cable`:

| Valor | Uso | Regra observada |
|---|---|---|
| `VBAudioCableWDM` | latência máxima (tamanho do buffer) | padrão 7168; aceito se ≥ 1024 |
| `VBAudioCableWDM_Latency` | latência atual | padrão = máxima; aceito se ≥ 48; limitado à máxima |
| `VBAudioCableWDM_SR` | taxa interna | padrão 96000; aceita 44100/48000/88200/176400/192000 |
| `VBAudioCableWDM_VolCtl` | aplicar volume do Windows | flag |
| `VBAudioCableWDM_LoopBack` | aloca um 2º buffer | flag |

**Buffer** (`0x140001008`): cabeçalho de 0x190 bytes + `(max_latency + 8) × 16 canais × 4 bytes`.
Ou seja, buffer circular interno de 16 canais com amostras de 32 bits.

**Leitura de disponibilidade** (`0x140001144`): `(escrita − leitura) mod tamanho`,
convertido pela razão entre a taxa do cliente e a taxa interna — o driver faz a
conversão de taxa ele mesmo.

**Cópia de escrita** (`0x1400011d4`): aceita clientes de 8000 a 200000 Hz, formatos
8/16/24/32 bits e float; usa um conversor de taxa quando a taxa do cliente difere da interna.

**Estatísticas** (`0x140006adc`, no caminho de reprodução): conta chamadas, histograma de
tamanho de buffer (<256, <512, <1024, ≥1024 amostras), erros de escrita, e um aviso quando
`3 × tamanho_do_buffer > latência` — o mesmo texto do painel:
*"Potential Error On Buffer size compared to current latency"*.

**IOCTLs** (`0x140003aa0`): o painel lê estatísticas (estrutura de 0x1a0 bytes), muda a
latência (mín. 48) e a taxa interna em tempo real.

## Inferido

- O comportamento é “o que entra na saída de áudio sai no microfone”, com o buffer
  circular como FIFO; *Push loss* = escrita sem espaço, *Pull loss* = leitura sem dados.
- O padrão “3× N amostras” do painel corresponde à regra latência ≥ 3 × buffer.

## Não determinado

- O efeito exato de `LoopBack` (cria um segundo buffer; uso não rastreado).
- Os detalhes do conversor de taxa e da política de recuperação após perda.

## Como foi recriado no Linux

| VB-CABLE (Windows) | NC-Cable (Linux) |
|---|---|
| Driver de kernel PortCls | Processo de usuário com dois nós PipeWire |
| Endpoint "CABLE Input" | Nó `Audio/Sink` "NC-Cable Input" |
| Endpoint "CABLE Output" | Nó `Audio/Source` "NC-Cable Output" |
| Buffer circular 16 canais × 32 bits | Ring buffer SPSC lock-free f32, 1–16 canais |
| Conversão de formato/taxa no driver | Feita pelo PipeWire para cada cliente |
| Registro (`VBAudioCableWDM_*`) | `~/.config/nc-cable/config.toml` |
| Painel de controle (IOCTL) | `nc-cable status` (JSON em `$XDG_RUNTIME_DIR`) |
| Volume do Windows (`VolCtl`) | Volume do nó PipeWire (sempre ativo) |
| Instalador + reboot | PKGBUILD / `install.sh` + serviço systemd de usuário |

Diferenças intencionais:

- Um módulo de kernel não é o caminho certo no Linux atual: o PipeWire é dono dos
  dispositivos e já oferece nós virtuais, e não há API de som do kernel em Rust.
- Taxa interna padrão 48000 (o grafo do PipeWire costuma rodar a 48 kHz) e latência
  padrão 1536 amostras (32 ms) em vez de 7168, que seria lenta demais para uso em tempo real.
- Os dois nós pedem o mesmo *driver* do grafo (`node.want-driver`), então compartilham
  o relógio e não há deriva; se houver acúmulo, o leitor descarta o excesso acima de
  2 × a latência (contado como *resync*).
- Vários cabos independentes por config, em vez das edições pagas A+B/C+D.
- `CABLE In 16 Ch` vira simplesmente `channels = 16`.
