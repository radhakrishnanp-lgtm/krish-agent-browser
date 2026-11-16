# CPU-only image. No model weights except the 2 MB Silero VAD file in server/assets.
FROM python:3.12-slim

ENV PYTHONUNBUFFERED=1 PIP_NO_CACHE_DIR=1
WORKDIR /app

COPY requirements.txt .
RUN pip install -r requirements.txt

COPY config ./config
COPY server ./server
COPY ui ./ui
COPY scripts ./scripts
RUN mkdir -p logs/sessions data/conversations && useradd -m app && chown -R app /app
USER app

EXPOSE 8080
CMD ["sh", "-c", "exec python -m uvicorn server.main:app --host 0.0.0.0 --port ${PORT:-8080} --no-access-log --ws-max-size 2097152 --proxy-headers --forwarded-allow-ips='*'"]
