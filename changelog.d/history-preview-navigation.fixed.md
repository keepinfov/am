Keep cached history-list navigation responsive during background transcript
refreshes, so Down followed by Enter opens the selected action's artifact rather
than the previous message. Foreground pagination remains serialized, and older
refresh replies cannot overwrite an explicit page or create spurious errors.
