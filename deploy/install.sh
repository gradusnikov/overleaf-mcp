#!/bin/bash
# Steps to deploy overleaf-mcp next to an Overleaf CE installation, run as root on the Overleaf host.
# Review and adapt before running; it changes the Overleaf compose project and the reverse proxy.
set -euo pipefail
D=/path/to/overleaf/compose/dir            # the directory with Overleaf's docker-compose.yml
AGENT=overleaf-agent@example.com           # Overleaf account the agents act as
SITE=https://overleaf.example.com          # public Overleaf URL

# 1. image: build it here, or on another machine and copy it:
#      docker save overleaf-mcp:0.1.0 | gzip | ssh root@overleaf-host 'gunzip | docker load'
docker image inspect overleaf-mcp:0.1.0 >/dev/null

# 2. a dedicated Overleaf user; this prints an activation link, open it and set a password
#    (it must not contain the e-mail's local part, Overleaf rejects that)
docker exec sharelatex bash -c "cd /overleaf/services/web && node modules/server-ce-scripts/scripts/create-user.mjs --email=$AGENT"

# 3. secrets, readable by the container user (nobody, uid/gid 65534) only
install -d -m 0750 -o root -g 65534 $D/mcp-secrets
read -r -s -p "Password set for $AGENT: " PW; echo
printf '%s\n' "$PW" > $D/mcp-secrets/overleaf-password
openssl rand -hex 32 > $D/mcp-secrets/mcp-tokens      # one token per line; one per client is best
chown root:65534 $D/mcp-secrets/*; chmod 0640 $D/mcp-secrets/*

# 4. add the service from compose-service.yml to $D/docker-compose.yml, then
cd $D && docker compose config -q && docker compose up -d --no-deps overleaf-mcp
curl -fsS http://127.0.0.1:5010/healthz

# 5. add apache-overleaf-mcp.conf to the Overleaf vhost, then
apachectl configtest && systemctl reload apache2
curl -s -o /dev/null -w 'unauthenticated: %{http_code} (expect 401)\n' -X POST $SITE/mcp
