struct MatchVM_PM {
    vm_id: u64,
    pm_addr: u64
}

// An external process will be responsible for (concurrent) constructing memory mappings 
// for snapshots of microVMs associated with all functions. 
// These snapshot memorys will be mapped and indexed in advance using unique sockets, serving as identifiers. 
// These memory mappings will be maintained and passed to the MicroVM restorer.

#[cfg(test)]
mod tests {
    
    #[test]
    fn test1() {   
        assert_eq!(2*2, 4);
    }
}
