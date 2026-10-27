repos <- Sys.getenv("R_REPOSITORIES", "https://cloud.r-project.org")
site_lib <- Sys.getenv("R_LIBS_SITE", "/opt/r-library")

# Restore into the library the app reads at runtime. When the project's
# .Rprofile activates renv, that is the renv-managed project library, so no
# explicit `library` argument is passed. When renv is not active (e.g. a
# hand-written renv.lock without renv::init), the project library is never
# used at runtime -- restore into R_LIBS_SITE instead, and warn loudly, since
# setup and runtime are being glued together without renv managing them.
# When the library is synchronized, restore is a fast no-op ("The library is
# already synchronized with the lockfile."), so running it on every launch
# doubles as the check.
renv_managed <- grepl("renv/library", .libPaths()[1], fixed = TRUE)

if (renv_managed) {
    renv::restore(
        lockfile = file.path(getwd(), "renv.lock"),
        confirm  = FALSE,
        clean    = FALSE
    )
} else {
    dir.create(site_lib, recursive = TRUE, showWarnings = FALSE)
    warning(
        "renv is not active for this project: restoring into R_LIBS_SITE (",
        site_lib,
        ") instead of the renv-managed project library. renv.lock is present ",
        "but the project does not activate renv (no renv .Rprofile) -- run ",
        "renv::init() to make renv manage this library.",
        call. = FALSE, immediate. = TRUE
    )
    renv::restore(
        library  = site_lib,
        lockfile = file.path(getwd(), "renv.lock"),
        confirm  = FALSE,
        clean    = FALSE
    )
}

# Suggests not covered by renv.lock (snapshot.type is "explicit"): install only
# what is actually missing, so an up-to-date library never hits CRAN.
if (file.exists("DESCRIPTION")) {
    desc <- read.dcf("DESCRIPTION")
    if ("Suggests" %in% colnames(desc)) {
        suggests <- trimws(strsplit(gsub("\\s*\\([^)]*\\)", "", desc[,"Suggests"]), ",")[[1]])
        suggests <- suggests[nzchar(suggests)]
        install_lib <- if (renv_managed) NULL else site_lib
        for (pkg in suggests) {
            if (!requireNamespace(pkg, quietly = TRUE)) {
                install.packages(pkg, repos = repos, lib = install_lib)
            }
        }
    }
}
