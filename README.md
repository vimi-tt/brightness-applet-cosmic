# Full Brightness

Applet de controle de brilho para o ambiente de desktop **COSMIC™** (Pop!_OS e outras distribuições Linux).

Permite gerenciar de forma independente o brilho de: (em fase de testes)
- **Telas internas de laptops**: via interface de `backlight`/`sysfs` e D-Bus `systemd-logind`.
- **Monitores externos de PC**: via protocolo de hardware **DDC/CI** sobre barramentos I2C.
- **Televisores (TVs) e telas sem DDC/CI**: via **modo software** (`xrandr`), permitindo ajustar o brilho de qualquer TV conectada por HDMI mesmo quando o hardware não suporta DDC/CI.
- Suporte a ajuste rápido direto via scroll da roda do mouse sobre o ícone do applet no painel.

---

## ⚠️ Requisito Importante: Monitores Externos (DDC/CI e I2C)

Para que o applet consiga controlar o brilho de **monitores externos** conectados via HDMI, DisplayPort ou USB-C, o sistema operacional precisa permitir o acesso de leitura e escrita aos nós de barramento `/dev/i2c-*`.

Por padrão, muitas distribuições Linux (como o Pop!_OS e o Ubuntu) criam esses dispositivos restritos ao usuário `root` (`crw------- root:root`), o que causa o seguinte aviso no console ou na interface ao mover o slider:

```
[cosmic-brightness-applet] Erro ao aplicar brilho: Acesso negado em /dev/i2c-*. Configure o grupo 'i2c' e as regras udev.
```

### Configuração de Permissões (Passo a Passo)

Execute os comandos abaixo no seu terminal para configurar o acesso:

#### 1. Instalar os pacotes necessários
Instale o `ddcutil` e o `i2c-tools`. O `ddcutil` instala nativamente as regras de `uaccess` para placas de vídeo e atua como camada de contingência no applet, enquanto o `i2c-tools` cria o grupo de sistema `i2c`:

```bash
sudo apt update && sudo apt install -y ddcutil i2c-tools
```

#### 2. Adicionar seu usuário ao grupo `i2c`
```bash
sudo usermod -aG i2c $USER
```

#### 3. Carregar o módulo de kernel `i2c-dev` na inicialização
```bash
echo i2c-dev | sudo tee /etc/modules-load.d/i2c-dev.conf
sudo modprobe i2c-dev
```

#### 4. Criar ou validar as regras udev
Crie o arquivo `/etc/udev/rules.d/99-i2c.rules`:

```bash
sudo tee /etc/udev/rules.d/99-i2c.rules << 'EOF'
# Permissão para o grupo de sistema 'i2c'
KERNEL=="i2c-[0-9]*", GROUP="i2c", MODE="0660"

# Permissão imediata via ACL (uaccess) para a sessão do usuário logado (seat0)
SUBSYSTEM=="i2c-dev", KERNEL=="i2c-[0-9]*", ATTRS{class}=="0x030000", TAG+="uaccess"
EOF
```

#### 5. Recarregar e disparar as regras do udev
```bash
sudo udevadm control --reload-rules && sudo udevadm trigger
```

> [!TIP]
> A regra `TAG+="uaccess"` libera o acesso ao barramento do monitor **imediatamente** para a sua sessão ativa, sem necessidade de reiniciar. Caso utilize somente a permissão por grupo `i2c`, encerre a sessão gráfica (logout) e faça login novamente para que o novo grupo seja reconhecido.

### Como testar se o acesso foi liberado

1. **Verifique as permissões de acesso ao barramento I2C** (ex.: `/dev/i2c-4`):
   ```bash
   getfacl /dev/i2c-4
   ```
   *(Deverá listar uma entrada como `user:<seu_usuario>:rw-`)*

2. **Teste a comunicação DDC/CI com o monitor**:
   ```bash
   ddcutil detect
   ```
   > [!NOTE]
   > Se o `ddcutil detect` exibir `DDC communication failed` ou `Invalid display`, significa que as permissões estão corretas, mas o display conectado (muito comum em **televisores**, adaptadores HDMI ou alguns monitores) não suporta ou está com o protocolo **DDC/CI** desativado em seu menu interno (OSD).
   >
   > **Nesses casos, o Full Brightness ativa automaticamente o Modo Software (`xrandr`)**, fornecendo controle de brilho fluido para a sua TV sem necessidade de suporte a DDC/CI!

---

## 🚀 Compilação e Instalação

Um [`justfile`](./justfile) está incluído para facilitar as etapas de desenvolvimento e empacotamento com o [just]:

- `just` (ou `just build-release`): Compila o applet com otimizações de release.
- `just run`: Compila e roda o applet imediatamente.
- `just install`: Instala os binários, ícones e arquivos de desktop no sistema.
- `just check`: Executa o linter `clippy` para verificação de boas práticas.
- `just check-json`: Verificação com saída em JSON (ideal para integração com IDEs / LSP).

### Empacotamento para Distribuições

Para distribuições Linux, utilize as receitas de dependências vendored:

```sh
just vendor
just build-vendored
just rootdir=debian/full-brightness prefix=/usr install
```

---

## 🌐 Tradução (i18n)

O projeto utiliza o [Fluent] para internacionalização. Os arquivos de tradução ficam no diretório [`i18n/`](./i18n). Para adicionar um novo idioma, crie a pasta correspondente ao código ISO da língua (ex: `i18n/pt_BR`) com base no modelo em inglês.

---

## 🛠️ Ambiente de Desenvolvimento

Recomenda-se instalar o Rust via [rustup] e configurar o editor com [rust-analyzer].

[fluent]: https://projectfluent.org/
[just]: https://github.com/casey/just
[rustup]: https://rustup.rs/
[rust-analyzer]: https://rust-analyzer.github.io/
