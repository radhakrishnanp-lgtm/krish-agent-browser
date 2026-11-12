FROM node:22-bookworm-slim

# Chromium (the real browser), TigerVNC (virtual screen), openbox (window manager),
# noVNC (browser-side viewer files), fonts, tini (clean process handling)
RUN apt-get update && apt-get install -y --no-install-recommends \
      chromium \
      tigervnc-standalone-server \
      openbox \
      novnc \
      fonts-liberation fonts-noto-core fonts-noto-color-emoji \
      ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --omit=dev && npm cache clean --force
COPY server.js ./
COPY public ./public
COPY docker/rc.xml /home/node/.config/openbox/rc.xml

# Browser downloads go to /data/files, which the web UI's Files tab can list and download
RUN mkdir -p /data/files && ln -s /data/files /home/node/Downloads \
    && chown -R node:node /data /home/node /app && chown -h node:node /home/node/Downloads
USER node

ENV NODE_ENV=production \
    PORT=3000 \
    DATA_DIR=/data \
    CHROME_BIN=/usr/bin/chromium \
    NOVNC_DIR=/usr/share/novnc

VOLUME /data
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=40s --retries=3 \
  CMD node -e "fetch('http://127.0.0.1:3000/healthz').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["node", "server.js"]
