# Optional external Hermes runtime. No owner settings or credentials belong here.
FROM python@sha256:a6e34c598f2467ed0e9a8d349809fcd8b5c603269512df273a0bb1784edc11b1
ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 PYTHONPATH=/opt/hermes
COPY requirements.txt /tmp/requirements.txt
RUN python -m pip install --no-cache-dir --require-hashes -r /tmp/requirements.txt
COPY source.tar /tmp/source.tar
RUN mkdir /opt/hermes && tar -xf /tmp/source.tar -C /opt/hermes && rm /tmp/source.tar
RUN python -m acp_adapter --check
WORKDIR /workspace
ENTRYPOINT ["python", "-m", "acp_adapter"]
