# Build context is the output of package.sh, not the development checkout.
# Binaries are prebuilt using Rust 1.99.1 through the mandatory sccache path.
FROM archlinux:base
RUN pacman -Syu --noconfirm --needed \
    bash ca-certificates coreutils curl file findutils fio gawk gcc-libs git \
    grep inetutils jq procps-ng shadow tar util-linux zstd && \
    useradd --create-home --uid 10001 bench && \
    mkdir -p /work/data /work/results /opt/all-db-bench && \
    chown -R bench:bench /work
COPY --chown=bench:bench . /opt/all-db-bench/
RUN ln -s /work/results /opt/all-db-bench/results
USER bench
WORKDIR /opt/all-db-bench
ENV RENTAL_WORK_ROOT=/work
ENTRYPOINT ["/opt/all-db-bench/deploy/rental/run.sh"]
